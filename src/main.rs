use acre::{acme::*, lsp, plumb};
use crossbeam_channel::{bounded, Receiver, Select};
use lsp_types::{notification::*, request::*, *};
use nine::p2000::OpenMode;
use serde::Deserialize;
use std::any::Any;
use std::collections::HashMap;
use std::fmt::Write;
use std::io::Read;
use std::thread;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

#[derive(Deserialize)]
struct TomlConfig {
	servers: Vec<ConfigServer>,
}

#[derive(Deserialize)]
struct ConfigServer {
	name: String,
	executable: Option<String>,
	extension: String,
	root_uri: Option<String>,
	workspace_folders: Option<Vec<String>>,
}

fn main() -> Result<()> {
	let dir = xdg::BaseDirectories::new()?;
	const ACRE_TOML: &str = "acre.toml";
	let config = match dir.find_config_file(ACRE_TOML) {
		Some(c) => c,
		None => {
			println!(
				"could not find {} in config location (maybe ~/.config/acre.toml)",
				ACRE_TOML,
			);
			std::process::exit(1);
		}
	};
	let config = std::fs::read_to_string(config)?;
	let config: TomlConfig = toml::from_str(&config)?;

	let mut clients = vec![];
	for server in config.servers {
		clients.push(lsp::Client::new(
			server.name.clone(),
			server.extension,
			server.executable.unwrap_or(server.name),
			std::iter::empty(),
			server.root_uri,
			server.workspace_folders,
		)?);
	}
	if clients.is_empty() {
		println!("empty servers in configuration file");
		std::process::exit(1);
	}
	let mut s = Server::new(clients)?;
	s.wait()
}

struct Server {
	w: Win,
	ws: HashMap<usize, ServerWin>,
	// Sorted Vec of (filenames, win id) to know which order to print windows in.
	names: Vec<(String, usize)>,
	// Vec of (position, win id) to map Look locations to windows.
	addr: Vec<(usize, usize)>,

	body: String,
	output: Vec<String>,
	focus: String,
	progress: HashMap<String, String>,
	// file name -> list of diagnostics
	diags: HashMap<String, Vec<String>>,

	log_r: Receiver<LogEvent>,
	ev_r: Receiver<Event>,
	err_r: Receiver<Error>,

	// client name -> client
	clients: HashMap<String, lsp::Client>,
	// client name -> capabilities
	capabilities: HashMap<String, lsp_types::ServerCapabilities>,
	// file name -> client name
	files: HashMap<String, String>,
}

struct ServerWin {
	name: String,
	w: Win,
	doc: TextDocumentIdentifier,
	url: Url,
	lang_id: String,
	version: i64,
	client: String,
}

impl ServerWin {
	fn new(name: String, w: Win, client: String) -> Result<ServerWin> {
		let url = Url::parse(&format!("file://{}", name))?;
		let doc = TextDocumentIdentifier::new(url.clone());
		let lang_id = match name.rsplit(".").next().unwrap_or("") {
			"go" => "go",
			"rs" => "rust",
			_ => panic!("unknown file extension {}", name),
		}
		.to_string();
		Ok(ServerWin {
			name,
			w,
			doc,
			url,
			lang_id,
			version: 1,
			client,
		})
	}
	fn pos(&mut self) -> Result<(usize, usize)> {
		self.w.ctl("addr=dot")?;
		// TODO: convert these character (rune) offsets to byte offsets.
		self.w.read_addr()
	}
	fn position(&mut self) -> Result<Position> {
		let pos = self.pos()?;
		let nl = NlOffsets::new(self.w.read(File::Body)?)?;
		let (line, col) = nl.offset_to_line(pos.0 as u64);
		Ok(Position::new(line, col))
	}
	fn text(&mut self) -> Result<(i64, String)> {
		let mut buf = String::new();
		self.w.read(File::Body)?.read_to_string(&mut buf)?;
		self.version += 1;
		Ok((self.version, buf))
	}
	fn change_params(&mut self) -> Result<DidChangeTextDocumentParams> {
		let (version, text) = self.text()?;
		Ok(DidChangeTextDocumentParams {
			text_document: VersionedTextDocumentIdentifier::new(self.url.clone(), version),
			content_changes: vec![TextDocumentContentChangeEvent {
				range: None,
				range_length: None,
				text,
			}],
		})
	}
	fn did_change(&mut self, client: &mut lsp::Client) -> Result<()> {
		client.notify::<DidChangeTextDocument>(self.change_params()?)
	}
	fn text_doc_pos(&mut self) -> Result<TextDocumentPositionParams> {
		let pos = self.position()?;
		Ok(TextDocumentPositionParams::new(self.doc.clone(), pos))
	}
}

impl Server {
	fn new(clients: Vec<lsp::Client>) -> Result<Server> {
		let (log_s, log_r) = bounded(0);
		let (ev_s, ev_r) = bounded(0);
		let (err_s, err_r) = bounded(0);
		let mut w = Win::new()?;
		w.name("acre")?;
		let mut wev = w.events()?;
		let mut cls = HashMap::new();
		for c in clients {
			let name = c.name.clone();
			cls.insert(name, c);
		}
		let s = Server {
			w,
			ws: HashMap::new(),
			names: vec![],
			addr: vec![],
			output: vec![],
			body: "".to_string(),
			focus: "".to_string(),
			progress: HashMap::new(),
			diags: HashMap::new(),
			log_r,
			ev_r,
			err_r,
			clients: cls,
			capabilities: HashMap::new(),
			files: HashMap::new(),
		};
		let err_s1 = err_s.clone();
		thread::Builder::new()
			.name("LogReader".to_string())
			.spawn(move || {
				let mut log = LogReader::new().unwrap();
				loop {
					match log.read() {
						Ok(ev) => match ev.op.as_str() {
							"new" | "del" | "focus" | "put" => {
								if cfg!(debug_assertions) {
									println!("log reader: {:?}", ev);
								}
								log_s.send(ev).unwrap();
							}
							_ => {
								if cfg!(debug_assertions) {
									println!("log reader: {:?} [uncaught]", ev);
								}
							}
						},
						Err(err) => {
							err_s1.send(err).unwrap();
							return;
						}
					};
				}
			})
			.unwrap();
		thread::Builder::new()
			.name("WindowEvents".to_string())
			.spawn(move || loop {
				let mut ev = wev.read_event().unwrap();
				match ev.c2 {
					'x' | 'X' => match ev.text.as_str() {
						"Del" => {
							return;
						}
						"Get" => {
							ev_s.send(ev).unwrap();
						}
						_ => {
							wev.write_event(ev).unwrap();
						}
					},
					'L' => {
						ev.load_text();
						ev_s.send(ev).unwrap();
					}
					_ => {}
				}
			})
			.unwrap();
		Ok(s)
	}
	fn sync(&mut self) -> Result<()> {
		let mut body = String::new();
		for (_, ds) in &self.diags {
			for d in ds {
				write!(&mut body, "{}\n", d)?;
			}
			if ds.len() > 0 {
				body.push('\n');
			}
		}
		self.addr.clear();
		for (file_name, id) in &self.names {
			self.addr.push((body.len(), *id));
			write!(
				&mut body,
				"{}{}\n\t",
				if *file_name == self.focus { "*" } else { "" },
				file_name
			)?;
			let client_name = self.files.get(file_name).unwrap();
			let caps = match self.capabilities.get(client_name) {
				Some(v) => v,
				None => continue,
			};
			if caps.definition_provider.unwrap_or(false) {
				body.push_str("[definition] ");
			}
			if caps.hover_provider.unwrap_or(false) {
				body.push_str("[hover] ");
			}
			if let Some(cap) = &caps.completion_provider {
				if cap.resolve_provider.unwrap_or(false) {
					body.push_str("[complete] ");
				}
			}
			if caps.references_provider.unwrap_or(false) {
				body.push_str("[references] ");
			}
			if caps.document_symbol_provider.unwrap_or(false) {
				body.push_str("[symbols] ");
			}
			body.push('\n');
		}
		self.addr.push((body.len(), 0));
		write!(&mut body, "-----\n")?;
		const MAX_LEN: usize = 1;
		if self.output.len() > MAX_LEN {
			self.output.drain(MAX_LEN..);
		}
		for s in &self.output {
			write!(&mut body, "\n{}\n", s)?;
		}
		if self.progress.len() > 0 {
			body.push('\n');
		}
		for (_, p) in &self.progress {
			write!(&mut body, "{}\n", p)?;
		}
		if self.body != body {
			self.body = body.clone();
			self.w.write(File::Addr, &format!(","))?;
			self.w.write(File::Data, &body)?;
			self.w.ctl("cleartag\nclean")?;
			self.w.write(File::Tag, " Get")?;
		}
		Ok(())
	}
	fn sync_windows(&mut self) -> Result<()> {
		let mut ws = HashMap::new();
		let mut wins = WinInfo::windows()?;
		self.names.clear();
		wins.sort_by(|a, b| a.name.cmp(&b.name));
		self.files.clear();
		for wi in wins {
			let mut client = None;
			for (_, c) in self.clients.iter_mut() {
				if wi.name.ends_with(&c.files) {
					// Don't open windows for a client that hasn't initialized yet.
					if !self.capabilities.contains_key(&c.name) {
						continue;
					}
					self.files.insert(wi.name.clone(), c.name.clone());
					client = Some(c);
					break;
				}
			}
			let client = match client {
				Some(c) => c,
				None => continue,
			};
			self.names.push((wi.name.clone(), wi.id));
			let w = match self.ws.remove(&wi.id) {
				Some(w) => w,
				None => {
					let mut fsys = FSYS.lock().unwrap();
					let ctl = fsys.open(format!("{}/ctl", wi.id).as_str(), OpenMode::RDWR)?;
					let w = Win::open(&mut fsys, wi.id, ctl)?;
					// Explicitly drop fsys here to remove its lock to prevent deadlocking if we
					// call w.events().
					drop(fsys);
					let mut sw = ServerWin::new(wi.name, w, client.name.clone())?;
					let (version, text) = sw.text()?;
					client.notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
						text_document: TextDocumentItem::new(
							sw.url.clone(),
							sw.lang_id.clone(),
							version,
							text,
						),
					})?;

					sw
				}
			};
			ws.insert(wi.id, w);
		}
		// close remaining files
		for (_, w) in &self.ws {
			let client = self.clients.get_mut(&w.client).unwrap();
			client.notify::<DidCloseTextDocument>(DidCloseTextDocumentParams {
				text_document: w.doc.clone(),
			})?;
		}
		self.ws = ws;
		Ok(())
	}
	fn lsp_msg(&mut self, client_name: String, msg: Box<dyn Any>) -> Result<()> {
		let client = &self.clients.get(&client_name).unwrap();
		if let Some(msg) = msg.downcast_ref::<lsp::ResponseError>() {
			self.output.insert(0, format!("{}", msg.message));
		} else if let Some(msg) = msg.downcast_ref::<lsp::WindowProgress>() {
			let name = format!("{}-{}", client.name, msg.id);
			if msg.done.unwrap_or(false) {
				self.progress.remove(&name);
			} else {
				let s = format!(
					"[{}%] {}: {} ({})",
					format_pct(msg.percentage),
					&name,
					msg.message.as_ref().unwrap_or(&"".to_string()),
					msg.title.as_ref().unwrap_or(&"".to_string()),
				);
				self.progress.insert(name, s);
			}
		} else if let Some(msg) = msg.downcast_ref::<lsp_types::ProgressParams>() {
			let name = format!("{}-{:?}", client.name, msg.token);
			match &msg.value {
				ProgressParamsValue::WorkDone(value) => match value {
					WorkDoneProgress::Begin(value) => {
						let s = format!(
							"[{}%] {}: {} ({})",
							format_pct(value.percentage),
							&name,
							value.message.as_ref().unwrap_or(&"".to_string()),
							value.title
						);
						self.progress.insert(name, s);
					}
					WorkDoneProgress::Report(_) => {
						// TODO: implement
					}
					WorkDoneProgress::End(_) => {
						self.progress.remove(&name);
					}
				},
			}
		} else if let Some(msg) = msg.downcast_ref::<lsp_types::PublishDiagnosticsParams>() {
			let mut v = vec![];
			let path = msg.uri.path();
			for p in &msg.diagnostics {
				let msg = p.message.lines().next().unwrap_or("");
				v.push(format!(
					"{}:{}: [{:?}] {}",
					path,
					p.range.start.line + 1,
					p.severity.unwrap_or(lsp_types::DiagnosticSeverity::Error),
					msg,
				));
			}
			self.diags.insert(path.to_string(), v);
		} else if let Some(msg) = msg.downcast_ref::<lsp_types::ShowMessageParams>() {
			self.output
				.insert(0, format!("[{:?}] {}", msg.typ, msg.message));
		} else if let Some(msg) = msg.downcast_ref::<InitializeResult>() {
			let client = self.clients.get_mut(&client_name).unwrap();
			client.notify::<Initialized>(InitializedParams {})?;
			self.capabilities
				.insert(client_name, msg.capabilities.clone());
			self.sync_windows()?;
		} else if let Some(msg) = msg.downcast_ref::<Option<GotoDefinitionResponse>>() {
			if let Some(msg) = msg {
				match msg {
					GotoDefinitionResponse::Array(locs) => match locs.len() {
						0 => {}
						1 => {
							let plumb = location_to_plumb(&locs[0]);
							plumb_location(plumb)?;
						}
						_ => {
							panic!("unknown definition response: {:?}", msg);
						}
					},
					_ => panic!("unknown definition response: {:?}", msg),
				};
			}
		} else if let Some(msg) = msg.downcast_ref::<Option<Hover>>() {
			if let Some(msg) = msg {
				match &msg.contents {
					HoverContents::Array(mss) => {
						let mut o: Vec<String> = vec![];
						for ms in mss {
							match ms {
								MarkedString::String(s) => o.push(s.clone()),
								MarkedString::LanguageString(s) => o.push(s.value.clone()),
							};
						}
						self.output.insert(0, o.join("\n"));
					}
					HoverContents::Markup(mc) => {
						self.output.insert(0, mc.value.clone());
					}
					_ => panic!("unknown hover response: {:?}", msg),
				};
			}
		} else if let Some(msg) = msg.downcast_ref::<Option<CompletionResponse>>() {
			if let Some(msg) = msg {
				let mut o: Vec<String> = vec![];
				match msg {
					CompletionResponse::Array(cis) => {
						for ci in cis {
							let mut s = ci.label.clone();
							if let Some(k) = ci.kind {
								write!(&mut s, " ({:?})", k)?;
							}
							if let Some(d) = &ci.detail {
								write!(&mut s, ": {}", d)?;
							}
							o.push(s);
						}
					}
					_ => panic!("unknown completion response: {:?}", msg),
				}
				if o.len() > 0 {
					let n = std::cmp::min(o.len(), 20);
					self.output.insert(0, o[0..n].join("\n"));
				}
			}
		} else if let Some(msg) = msg.downcast_ref::<Option<Vec<Location>>>() {
			if let Some(msg) = msg {
				let o: Vec<String> = msg.into_iter().map(|x| location_to_plumb(x)).collect();
				if o.len() > 0 {
					self.output.insert(0, o.join("\n"));
				}
			}
		} else if let Some(msg) = msg.downcast_ref::<Option<DocumentSymbolResponse>>() {
			if let Some(msg) = msg {
				let mut o: Vec<String> = vec![];
				match msg {
					DocumentSymbolResponse::Flat(sis) => {
						for si in sis {
							// Ignore variables in methods.
							if si.container_name.as_ref().unwrap_or(&"".to_string()).len() == 0
								&& si.kind == SymbolKind::Variable
							{
								continue;
							}
							o.push(format!(
								"{}{} ({:?}): {}",
								if let Some(c) = &si.container_name {
									if c.len() > 0 {
										format!("{}.", c)
									} else {
										"".to_string()
									}
								} else {
									"".to_string()
								},
								si.name,
								si.kind,
								location_to_plumb(&si.location),
							));
						}
					}
					DocumentSymbolResponse::Nested(dss) => {
						// TODO: handle nesting.
						for ds in dss {
							o.push(format! {
								"{}{} ({:?}) :{}",
								ds.name,
								match &ds.detail {
									Some(d) => format!(": {}", d),
									None => "".to_string(),
								},
								ds.kind,
								ds.range.start.line+1,
							});
						}
					}
				}
				if o.len() > 0 {
					self.output.insert(0, o.join("\n"));
				}
			}
		} else {
			// TODO: how do we get the underlying struct here so we
			// know which message we are missing?
			panic!("unrecognized msg: {:?}", msg);
		}
		Ok(())
	}
	fn run_cmd(&mut self, ev: Event) -> Result<()> {
		match ev.c2 {
			'x' | 'X' => match ev.text.as_str() {
				"Get" => {
					self.output.clear();
					self.sync_windows()?;
				}
				_ => {
					panic!("unexpected");
				}
			},
			'L' => {
				let mut wid: usize = 0;
				for (pos, id) in self.addr.iter().rev() {
					if (*pos as u32) < ev.q0 {
						wid = *id;
						break;
					}
				}
				if wid == 0 {
					return plumb_location(ev.text);
				}
				let sw = self.ws.get_mut(&wid).unwrap();
				let client = self
					.clients
					.get_mut(self.files.get(&sw.name).unwrap())
					.unwrap();
				sw.did_change(client)?;
				match ev.text.as_str() {
					"definition" => {
						client.send::<GotoDefinition>(sw.text_doc_pos()?)?;
					}
					"hover" => {
						client.send::<HoverRequest>(sw.text_doc_pos()?)?;
					}
					"complete" => {
						client.send::<Completion>(CompletionParams {
							text_document_position: sw.text_doc_pos()?,
							work_done_progress_params: WorkDoneProgressParams {
								work_done_token: None,
							},
							partial_result_params: PartialResultParams {
								partial_result_token: None,
							},
							context: Some(CompletionContext {
								trigger_kind: CompletionTriggerKind::Invoked,
								trigger_character: None,
							}),
						})?;
					}
					"references" => {
						client.send::<References>(ReferenceParams {
							text_document_position: sw.text_doc_pos()?,
							work_done_progress_params: WorkDoneProgressParams {
								work_done_token: None,
							},
							context: ReferenceContext {
								include_declaration: true,
							},
						})?;
					}
					"symbols" => {
						client.send::<DocumentSymbolRequest>(DocumentSymbolParams {
							text_document: TextDocumentIdentifier::new(sw.url.clone()),
						})?;
					}
					_ => panic!("unexpected text {}", ev.text),
				};
			}
			_ => {}
		}
		Ok(())
	}
	fn cmd_put(&mut self, id: usize) -> Result<()> {
		let sw = if let Some(sw) = self.ws.get_mut(&id) {
			sw
		} else {
			// Ignore unknown ids (untracked files, zerox, etc.).
			return Ok(());
		};
		let client = self.clients.get_mut(&sw.client).unwrap();
		sw.did_change(client)?;
		client.notify::<DidSaveTextDocument>(DidSaveTextDocumentParams {
			text_document: sw.doc.clone(),
		})?;
		Ok(())
	}
	fn wait(&mut self) -> Result<()> {
		self.sync_windows()?;
		// chan index -> (recv chan, self.clients index)

		// one-time index setup
		let mut sel = Select::new();
		let sel_log_r = sel.recv(&self.log_r);
		let sel_ev_r = sel.recv(&self.ev_r);
		let sel_err_r = sel.recv(&self.err_r);
		let mut clients = HashMap::new();

		for (name, c) in &self.clients {
			clients.insert(sel.recv(&c.msg_r), (c.msg_r.clone(), name.to_string()));
		}
		drop(sel);

		let mut no_sync = false;
		loop {
			if !no_sync {
				self.sync()?;
			}
			no_sync = false;

			let mut sel = Select::new();
			sel.recv(&self.log_r);
			sel.recv(&self.ev_r);
			sel.recv(&self.err_r);
			for (_, c) in &self.clients {
				sel.recv(&c.msg_r);
			}
			let index = sel.ready();

			match index {
				_ if index == sel_log_r => match self.log_r.recv() {
					Ok(ev) => match ev.op.as_str() {
						"focus" => {
							self.focus = ev.name;
						}
						"put" => {
							self.cmd_put(ev.id)?;
							no_sync = true;
						}
						"new" | "del" => {
							self.sync_windows()?;
						}
						_ => {
							panic!("unknown event op {:?}", ev);
						}
					},
					Err(_) => {
						break;
					}
				},
				_ if index == sel_ev_r => match self.ev_r.recv() {
					Ok(ev) => {
						self.run_cmd(ev)?;
					}
					Err(_) => {
						break;
					}
				},
				_ if index == sel_err_r => match self.err_r.recv() {
					Ok(_) => {
						break;
					}
					Err(_) => {
						break;
					}
				},
				_ => {
					let (ch, name) = clients.get(&index).unwrap();
					let msg = ch.recv()?;
					self.lsp_msg(name.to_string(), msg)?;
				}
			};
		}
		Ok(())
	}
}

impl Drop for Server {
	fn drop(&mut self) {
		let _ = self.w.del(true);
	}
}

fn location_to_plumb(l: &Location) -> String {
	format!("{}:{}", l.uri.path(), l.range.start.line + 1,)
}

fn plumb_location(loc: String) -> Result<()> {
	let f = plumb::open("send", OpenMode::WRITE)?;
	let msg = plumb::Message {
		dst: "edit".to_string(),
		typ: "text".to_string(),
		data: loc.into(),
	};
	return msg.send(f);
}

fn format_pct(pct: Option<f64>) -> String {
	match pct {
		Some(v) => format!("{:.0}", v),
		None => "?".to_string(),
	}
}
