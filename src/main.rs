use acre::{acme::*, err_str, lsp, plumb};
use crossbeam_channel::{bounded, Receiver, Select};
use diff;
use lazy_static::lazy_static;
use lsp_types::{notification::*, request::*, *};
use nine::p2000::OpenMode;
use regex::Regex;
use serde::Deserialize;
use std::any::Any;
use std::collections::HashMap;
use std::fmt::Write;
use std::fs::metadata;
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
	files: String,
	root_uri: Option<String>,
	workspace_folders: Option<Vec<String>>,
}

fn main() -> Result<()> {
	let dir = xdg::BaseDirectories::new()?;
	const ACRE_TOML: &str = "acre.toml";
	let config = match dir.find_config_file(ACRE_TOML) {
		Some(c) => c,
		None => {
			let mut path = dir.get_config_home();
			path.push(ACRE_TOML);
			eprintln!("could not find {}", path.to_str().unwrap());
			std::process::exit(1);
		}
	};
	let config = std::fs::read_to_string(config)?;
	let config: TomlConfig = toml::from_str(&config)?;

	let mut clients = vec![];
	for server in config.servers {
		clients.push(lsp::Client::new(
			server.name.clone(),
			server.files,
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

struct Progress {
	name: String,
	percentage: Option<f64>,
	message: Option<String>,
	title: String,
}

impl Progress {
	fn new(
		name: String,
		percentage: Option<f64>,
		message: Option<String>,
		title: Option<String>,
	) -> Self {
		Progress {
			name,
			percentage,
			message,
			title: title.unwrap_or("".to_string()),
		}
	}
}

impl std::fmt::Display for Progress {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"[{}%] {}:{} ({})",
			format_pct(self.percentage),
			self.name,
			if let Some(msg) = &self.message {
				format!(" {}", msg)
			} else {
				"".to_string()
			},
			self.title,
		)
	}
}

#[derive(Debug, Clone)]
enum Action {
	Command(CodeActionOrCommand),
	Completion(CompletionItem),
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
struct ClientId {
	client_name: String,
	msg_id: usize,
}

impl ClientId {
	fn new<S: Into<String>>(client_name: S, msg_id: usize) -> Self {
		ClientId {
			client_name: client_name.into(),
			msg_id,
		}
	}
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
	progress: HashMap<String, Progress>,
	// file name -> list of diagnostics
	diags: HashMap<String, Vec<String>>,
	// request (client_name, id) -> file Url
	requests: HashMap<ClientId, Url>,
	actions: HashMap<ClientId, Vec<Action>>,
	// Vec of position and (ClientId, index) into the vec of actions.
	action_addrs: Vec<(usize, (ClientId, usize))>,

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
	version: i64,
	client: String,
}

impl ServerWin {
	fn new(name: String, w: Win, client: String) -> Result<ServerWin> {
		let url = Url::parse(&format!("file://{}", name))?;
		let doc = TextDocumentIdentifier::new(url.clone());
		Ok(ServerWin {
			name,
			w,
			doc,
			url,
			version: 1,
			client,
		})
	}
	fn pos(&mut self) -> Result<(usize, usize)> {
		self.w.ctl("addr=dot")?;
		// TODO: convert these character (rune) offsets to byte offsets.
		self.w.read_addr()
	}
	fn nl(&mut self) -> Result<NlOffsets> {
		NlOffsets::new(self.w.read(File::Body)?)
	}
	fn position(&mut self) -> Result<Position> {
		let pos = self.pos()?;
		let nl = self.nl()?;
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
			requests: HashMap::new(),
			actions: HashMap::new(),
			action_addrs: vec![],
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
	fn get_sw_by_url(&mut self, url: &Url) -> Result<&mut ServerWin> {
		let filename = url.path();
		let mut wid: Option<usize> = None;
		for (name, id) in &self.names {
			if filename == name {
				wid = Some(*id);
				break;
			}
		}
		let wid = match wid {
			Some(id) => id,
			None => return Err(err_str(format!("could not find file {}", filename))),
		};
		let sw = match self.ws.get_mut(&wid) {
			Some(sw) => sw,
			None => return Err(err_str(format!("could not find window {}", wid))),
		};
		Ok(sw)
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
			#[cfg(debug_assertions)]
			{
				if caps.code_action_provider.is_some() {
					body.push_str("[assist] ");
				}
			}
			if caps.completion_provider.is_some() {
				body.push_str("[complete] ");
			}
			if caps.definition_provider.unwrap_or(false) {
				body.push_str("[definition] ");
			}
			if caps.hover_provider.unwrap_or(false) {
				body.push_str("[hover] ");
			}
			if caps.implementation_provider.is_some() {
				body.push_str("[impl] ");
			}
			#[cfg(debug_assertions)]
			{
				if caps.code_lens_provider.is_some() {
					body.push_str("[lens] ");
				}
			}
			if caps.references_provider.unwrap_or(false) {
				body.push_str("[references] ");
			}
			if caps.document_symbol_provider.unwrap_or(false) {
				body.push_str("[symbols] ");
			}
			if caps.signature_help_provider.is_some() {
				body.push_str("[signature] ");
			}
			if caps.type_definition_provider.is_some() {
				body.push_str("[typedef] ");
			}
			body.push('\n');
		}
		self.addr.push((body.len(), 0));
		write!(&mut body, "-----\n")?;
		const MAX_LEN: usize = 1;
		if self.output.len() > MAX_LEN {
			self.output.drain(MAX_LEN..);
		}
		self.action_addrs.clear();
		for (client_id, actions) in &self.actions {
			for (idx, action) in actions.iter().enumerate() {
				self.action_addrs
					.push((body.len(), (client_id.clone(), idx)));
				match action {
					Action::Command(CodeActionOrCommand::Command(cmd)) => {
						write!(&mut body, "\n[{}]", cmd.title)?;
					}
					Action::Command(CodeActionOrCommand::CodeAction(action)) => {
						write!(&mut body, "\n[{}]", action.title)?;
					}
					Action::Completion(item) => {
						write!(&mut body, "\n[insert] {}:", item.label)?;
						if item.deprecated.unwrap_or(false) {
							write!(&mut body, " DEPRECATED")?;
						}
						if let Some(k) = item.kind {
							write!(&mut body, " ({:?})", k)?;
						}
						if let Some(d) = &item.detail {
							write!(&mut body, " {}", d)?;
						}
					}
				}
			}
			write!(&mut body, "\n")?;
		}
		self.action_addrs
			.push((body.len(), (ClientId::new("", 0), 100000)));
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
				if c.files.is_match(&wi.name) {
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
							"".to_string(), // lang id
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
	fn lsp_msg(
		&mut self,
		client_name: String,
		msg_id: Option<usize>,
		msg: Box<dyn Any>,
	) -> Result<()> {
		let client = &self.clients.get(&client_name).unwrap();
		let client_id = match msg_id {
			Some(id) => Some(ClientId::new(client_name.clone(), id)),
			None => None,
		};
		let url = match client_id {
			Some(ref client_id) => match self.requests.get(client_id) {
				Some(url) => Some(url.clone()),
				None => None,
			},
			None => None,
		};
		if let Some(msg) = msg.downcast_ref::<lsp::ResponseError>() {
			self.output.insert(0, format!("{}", msg.message));
		} else if let Some(msg) = msg.downcast_ref::<lsp::WindowProgress>() {
			let name = format!("{}-{}", client.name, msg.id);
			if msg.done.unwrap_or(false) {
				self.progress.remove(&name);
			} else {
				self.progress.insert(
					name.clone(),
					Progress::new(name, msg.percentage, msg.message.clone(), msg.title.clone()),
				);
			}
		} else if let Some(msg) = msg.downcast_ref::<lsp_types::ProgressParams>() {
			let name = format!("{}-{:?}", client.name, msg.token);
			match &msg.value {
				ProgressParamsValue::WorkDone(value) => match value {
					WorkDoneProgress::Begin(value) => {
						self.progress.insert(
							name.clone(),
							Progress::new(
								name,
								value.percentage,
								value.message.clone(),
								Some(value.title.clone()),
							),
						);
					}
					WorkDoneProgress::Report(value) => {
						let p = self.progress.get_mut(&name).unwrap();
						p.percentage = value.percentage;
						p.message = value.message.clone();
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
		} else if let Some(msg) = msg.downcast_ref::<lsp_types::LogMessageParams>() {
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
				goto_definition(msg)?;
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
				self.actions.clear();
				let actions = match msg {
					CompletionResponse::Array(cis) => cis,
					CompletionResponse::List(cls) => &cls.items,
				};
				let mut v = vec![];
				for a in actions.iter().cloned() {
					v.push(Action::Completion(a));
				}
				v.truncate(10);
				self.actions.insert(client_id.unwrap(), v);
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
				fn add_symbol(
					o: &mut Vec<String>,
					container: &Vec<String>,
					name: &String,
					kind: SymbolKind,
					loc: &Location,
				) {
					o.push(format!(
						"{}{} ({:?}): {}",
						container
							.iter()
							.map(|c| format!("{}::", c))
							.collect::<Vec<String>>()
							.join(""),
						name,
						kind,
						location_to_plumb(loc),
					));
				};
				match msg.clone() {
					DocumentSymbolResponse::Flat(sis) => {
						for si in sis {
							// Ignore variables in methods.
							if si.container_name.as_ref().unwrap_or(&"".to_string()).len() == 0
								&& si.kind == SymbolKind::Variable
							{
								continue;
							}
							let cn = match si.container_name.clone() {
								Some(c) => vec![c],
								None => vec![],
							};
							add_symbol(&mut o, &cn, &si.name, si.kind, &si.location);
						}
					}
					DocumentSymbolResponse::Nested(mut dss) => {
						fn process(
							url: &Url,
							mut o: &mut Vec<String>,
							parents: &Vec<String>,
							dss: &mut Vec<DocumentSymbol>,
						) {
							dss.sort_by(|a, b| a.range.start.line.cmp(&b.range.start.line));
							for ds in dss {
								add_symbol(
									&mut o,
									parents,
									&ds.name,
									ds.kind,
									&Location::new(url.clone(), ds.range),
								);
								if let Some(mut children) = ds.children.clone() {
									let mut parents = parents.clone();
									parents.push(ds.name.clone());
									process(url, o, &parents, &mut children);
								}
							}
						};
						process(&url.unwrap(), &mut o, &vec![], &mut dss);
					}
				}
				if o.len() > 0 {
					self.output.insert(0, o.join("\n"));
				}
			}
		} else if let Some(msg) = msg.downcast_ref::<Option<SignatureHelp>>() {
			if let Some(msg) = msg {
				let mut o: Vec<String> = vec![];
				for sig in &msg.signatures {
					o.push(sig.label.clone());
				}
				if o.len() > 0 {
					self.output.insert(0, o.join("\n"));
				}
			}
		} else if let Some(msg) = msg.downcast_ref::<Option<Vec<CodeLens>>>() {
			if let Some(msg) = msg {
				let mut o: Vec<String> = vec![];
				let url = url.unwrap();
				for lens in msg {
					let loc = Location {
						uri: url.clone(),
						range: lens.range,
					};
					o.push(format!("{}", location_to_plumb(&loc)));
				}
				if o.len() > 0 {
					self.output.insert(0, o.join("\n"));
				}
			}
		} else if let Some(msg) = msg.downcast_ref::<Option<CodeActionResponse>>() {
			if let Some(msg) = msg {
				self.actions.clear();
				let mut v = vec![];
				for m in msg.iter().cloned() {
					v.push(Action::Command(m));
				}
				self.actions.insert(client_id.clone().unwrap(), v);
			}
		} else if let Some(msg) = msg.downcast_ref::<Option<Vec<TextEdit>>>() {
			if let Some(msg) = msg {
				self.apply_text_edits(&url.unwrap(), InsertTextFormat::PlainText, msg)?;
			}
		} else if let Some(msg) = msg.downcast_ref::<Option<GotoImplementationResponse>>() {
			if let Some(msg) = msg {
				goto_definition(msg)?;
			}
		} else if let Some(msg) = msg.downcast_ref::<Option<GotoTypeDefinitionResponse>>() {
			if let Some(msg) = msg {
				goto_definition(msg)?;
			}
		} else {
			// TODO: how do we get the underlying struct here so we
			// know which message we are missing?
			panic!("unrecognized msg: {:?}", msg);
		}
		Ok(())
	}
	fn apply_text_edits(
		&mut self,
		url: &Url,
		format: InsertTextFormat,
		edits: &Vec<TextEdit>,
	) -> Result<()> {
		if edits.is_empty() {
			return Ok(());
		}
		let sw = self.get_sw_by_url(url)?;
		let mut body = String::new();
		sw.w.read(File::Body)?.read_to_string(&mut body)?;
		let offsets = NlOffsets::new(std::io::Cursor::new(body.clone()))?;
		if edits.len() == 1 {
			if body == edits[0].new_text {
				return Ok(());
			}
			// Check if this is a full file replacement. If so, use a diff algorithm so acme doesn't scroll to the bottom.
			let edit = edits[0].clone();
			let last = offsets.last();
			if edit.range.start == Position::new(0, 0)
				&& edit.range.end == Position::new(last.0, last.1)
			{
				let lines = diff::lines(&body, &edit.new_text);
				let mut i = 0;
				for line in lines.iter() {
					i += 1;
					match line {
						diff::Result::Left(_) => {
							sw.w.addr(&format!("{},{}", i, i))?;
							sw.w.write(File::Data, "")?;
							i -= 1;
						}
						diff::Result::Right(s) => {
							sw.w.addr(&format!("{}+#0", i - 1))?;
							sw.w.write(File::Data, &format!("{}\n", s))?;
						}
						diff::Result::Both(_, _) => {}
					}
				}
				return Ok(());
			}
		}
		sw.w.seek(File::Body, std::io::SeekFrom::Start(0))?;
		sw.w.ctl("nomark")?;
		sw.w.ctl("mark")?;
		let mut delta: i64 = 0;
		for edit in edits.iter().rev() {
			let soff =
				offsets.line_to_offset(edit.range.start.line, edit.range.start.character) as i64;
			let eoff = offsets.line_to_offset(edit.range.end.line, edit.range.end.character) as i64;
			let addr = format!("#{},#{}", soff + delta, eoff + delta);
			sw.w.addr(&addr)?;
			let n = match format {
				InsertTextFormat::Snippet => {
					lazy_static! {
						static ref SNIPPET: Regex =
							Regex::new(r"(\$\{\d+:[[:alpha:]]+\})|(\$0)").unwrap();
					}
					let text = &SNIPPET.replace_all(&edit.new_text, "");
					sw.w.write(File::Data, text)?;
					text.len()
				}
				InsertTextFormat::PlainText => {
					sw.w.write(File::Data, &edit.new_text)?;
					edit.new_text.len()
				}
			} as i64;
			delta += n - (eoff - soff);
		}
		Ok(())
	}
	fn run_event(&mut self, ev: Event, wid: usize) -> Result<()> {
		let sw = self.ws.get_mut(&wid).unwrap();
		let client_name = self.files.get(&sw.name).unwrap();
		let client = self.clients.get_mut(client_name).unwrap();
		sw.did_change(client)?;
		let id;
		match ev.text.as_str() {
			"definition" => {
				id = client.send::<GotoDefinition>(sw.text_doc_pos()?)?;
			}
			"hover" => {
				id = client.send::<HoverRequest>(sw.text_doc_pos()?)?;
			}
			"complete" => {
				id = client.send::<Completion>(CompletionParams {
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
				id = client.send::<References>(ReferenceParams {
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
				id = client.send::<DocumentSymbolRequest>(DocumentSymbolParams {
					text_document: TextDocumentIdentifier::new(sw.url.clone()),
				})?;
			}
			"signature" => {
				id = client.send::<SignatureHelpRequest>(sw.text_doc_pos()?)?;
			}
			"lens" => {
				id = client.send::<CodeLensRequest>(CodeLensParams {
					text_document: TextDocumentIdentifier::new(sw.url.clone()),
					work_done_progress_params: WorkDoneProgressParams {
						work_done_token: None,
					},
					partial_result_params: PartialResultParams {
						partial_result_token: None,
					},
				})?;
			}
			"assist" => {
				let pos = sw.position()?;
				id = client.send::<CodeActionRequest>(CodeActionParams {
					text_document: TextDocumentIdentifier::new(sw.url.clone()),
					range: Range {
						start: pos,
						end: pos,
					},
					context: CodeActionContext {
						diagnostics: vec![],
						only: None,
					},
					work_done_progress_params: WorkDoneProgressParams {
						work_done_token: None,
					},
					partial_result_params: PartialResultParams {
						partial_result_token: None,
					},
				})?;
			}
			"impl" => {
				id = client.send::<GotoImplementation>(sw.text_doc_pos()?)?;
			}
			"typedef" => {
				id = client.send::<GotoTypeDefinition>(sw.text_doc_pos()?)?;
			}
			_ => return Ok(()),
		};
		self.requests
			.insert(ClientId::new(client_name, id), sw.url.clone());
		Ok(())
	}
	fn run_code_action(&mut self, client_id: ClientId, idx: usize) -> Result<()> {
		let url = self.requests.get(&client_id).unwrap().clone();
		let action = &self.actions.get(&client_id).unwrap()[idx].clone();
		self.actions.clear();
		match action {
			Action::Command(CodeActionOrCommand::Command(_cmd)) => panic!("unsupported"),
			Action::Command(CodeActionOrCommand::CodeAction(action)) => {
				if let Some(edit) = action.edit.clone() {
					println!("edit: {:?}", edit);
				}
			}
			Action::Completion(item) => {
				let format = item
					.insert_text_format
					.unwrap_or(InsertTextFormat::PlainText);
				if let Some(edit) = item.text_edit.clone() {
					return self.apply_text_edits(&url, format, &vec![edit]);
				}
				panic!("unsupported");
			}
		}
		Ok(())
	}
	fn run_cmd(&mut self, ev: Event) -> Result<()> {
		match ev.c2 {
			'x' | 'X' => match ev.text.as_str() {
				"Get" => {
					self.actions.clear();
					self.output.clear();
					self.sync_windows()?;
					self.diags.clear();
				}
				_ => {
					panic!("unexpected");
				}
			},
			'L' => {
				{
					let mut wid = 0;
					for (pos, id) in self.addr.iter().rev() {
						if (*pos as u32) < ev.q0 {
							wid = *id;
							break;
						}
					}
					if wid != 0 {
						return self.run_event(ev, wid);
					}
				}
				{
					let mut cid: Option<(ClientId, usize)> = None;
					for (pos, (client_id, idx)) in self.action_addrs.iter().rev() {
						if (*pos as u32) < ev.q0 && client_id.msg_id != 0 {
							cid = Some((client_id.clone(), *idx));
							break;
						}
					}
					if let Some((cid, idx)) = cid {
						return self.run_code_action(cid, idx);
					}
				}
				return plumb_location(ev.text);
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
		// TODO: make a common send method so requests is populated the
		// same here and in run_event.
		let capabilities = self.capabilities.get(&sw.client).unwrap();
		if capabilities.document_formatting_provider.unwrap_or(false) {
			let format_req_id = client.send::<Formatting>(DocumentFormattingParams {
				text_document: sw.doc.clone(),
				options: FormattingOptions {
					tab_size: 4,
					insert_spaces: false,
					properties: HashMap::new(),
					trim_trailing_whitespace: Some(true),
					insert_final_newline: Some(true),
					trim_final_newlines: Some(true),
				},
				work_done_progress_params: WorkDoneProgressParams {
					work_done_token: None,
				},
			})?;
			self.requests.insert(
				ClientId::new(sw.client.clone(), format_req_id),
				sw.url.clone(),
			);
		}
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
					let (id, msg) = ch.recv()?;
					self.lsp_msg(name.to_string(), id, msg)?;
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

fn goto_definition(goto: &GotoDefinitionResponse) -> Result<()> {
	match goto {
		GotoDefinitionResponse::Array(locs) => match locs.len() {
			0 => {}
			_ => {
				let plumb = location_to_plumb(&locs[0]);
				plumb_location(plumb)?;
			}
		},
		_ => panic!("unknown definition response: {:?}", goto),
	};
	Ok(())
}

fn location_to_plumb(l: &Location) -> String {
	format!("{}:{}", l.uri.path(), l.range.start.line + 1,)
}

fn plumb_location(loc: String) -> Result<()> {
	let path = loc.split(":").next().unwrap();
	// Verify path exists. If not, do nothing.
	if metadata(path).is_err() {
		return Ok(());
	}
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
