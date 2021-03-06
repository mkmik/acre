use std::collections::HashMap;
use std::fmt::Debug;
use std::io::{Cursor, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{bail, Result};
use byteorder::{LittleEndian, WriteBytesExt};
use crossbeam_channel::{bounded, Receiver, Sender};
use nine::{de::*, p2000::*, ser::*};

use crate::{fid, fsys};

#[derive(Clone)]
pub struct Conn {
    writer: Arc<Mutex<ConnWriter>>,
    pub msize: u32,
    tag_map: Arc<Mutex<HashMap<u16, Sender<Vec<u8>>>>>,
}

struct ConnWriter {
    msg_buf: Vec<u8>,
    stream: UnixStream,
    nextfid: u32,
    next_tag: u16,
    free_tags: Vec<u16>,
}

impl Conn {
    pub fn new(stream: UnixStream) -> Result<Self> {
        let mut reader = stream.try_clone()?;
        let mut c = Conn {
            writer: Arc::new(Mutex::new(ConnWriter {
                msg_buf: Vec::new(),
                stream,
                nextfid: 1,
                next_tag: 0,
                free_tags: vec![],
            })),
            msize: 131072,
            tag_map: Arc::new(Mutex::new(HashMap::new())),
        };
        let tm = Arc::clone(&c.tag_map);
        let cw = Arc::clone(&c.writer);

        thread::spawn(move || loop {
            let mut size: u32 = Conn::read_a(&reader).unwrap();
            let mtype: u8 = Conn::read_a(&reader).unwrap();
            size -= 5;
            let mut data = vec![0u8; size as usize];
            reader.read_exact(&mut data).unwrap();
            // Prepend the size back. The read_msg function needs
            // it incase an error type is returned.
            // TODO: is there a way to do this that doesn't involve
            // shifting everything to the right?
            data.insert(0, mtype);
            let tag: u16 = Conn::read_a(&data[1..3]).unwrap();
            let s = tm
                .lock()
                .unwrap()
                .remove(&tag)
                .expect(format!("expected receiver with tag {:?}", tag).as_str());
            cw.lock().unwrap().free_tags.push(tag);
            s.send(data).unwrap();
        });

        let (tag, r) = c.new_tag()?;
        let tx = Tversion {
            tag: tag,
            msize: c.msize,
            version: "9P2000".into(),
        };
        let rx = c.rpc::<Tversion, Rversion>(&tx, r)?;
        if rx.msize > c.msize {
            bail!("invalid msize {}", rx.msize);
        }
        c.msize = rx.msize;
        if rx.version != "9P2000" {
            bail!("invalid version {}", rx.version);
        }

        Ok(c)
    }

    fn new_tag(&mut self) -> Result<(u16, Receiver<Vec<u8>>)> {
        let mut cw = self.writer.lock().unwrap();
        let tag: u16;
        if cw.free_tags.len() > 0 {
            tag = cw.free_tags.remove(0);
        } else if cw.next_tag == NOTAG {
            bail!("out of tags");
        } else {
            tag = cw.next_tag;
            cw.next_tag += 1;
        }
        let (s, r) = bounded(0);
        self.tag_map.lock().unwrap().insert(tag, s);
        Ok((tag, r))
    }

    fn rpc<
        'de,
        S: Serialize + MessageTypeId + Debug,
        D: Deserialize<'de> + MessageTypeId + Debug,
    >(
        &mut self,
        s: &S,
        r: Receiver<Vec<u8>>,
    ) -> Result<D> {
        self.send_msg(s)?;
        self.read_msg::<D>(r)
    }

    fn send_msg<T: Serialize + MessageTypeId + Debug>(&mut self, t: &T) -> Result<()> {
        let mut cw = self.writer.lock().unwrap();
        cw.msg_buf.truncate(0);
        let amt = into_vec(&t, &mut cw.msg_buf)?;

        assert!(self.msize >= amt);
        cw.stream.write_u32::<LittleEndian>(amt + 5)?;
        cw.stream.write_u8(<T as MessageTypeId>::MSG_TYPE_ID)?;
        // Avoid a reference immutable/mutable borrowing problem.
        let mut stream = &cw.stream;
        Ok(stream.write_all(&cw.msg_buf[0..amt as usize])?)
    }

    fn read_msg<'de, T: Deserialize<'de> + MessageTypeId + Debug>(
        &mut self,
        r: Receiver<Vec<u8>>,
    ) -> Result<T> {
        let v = r.recv()?;
        let mut rv = Cursor::new(v);
        let mtype: u8 = Conn::read_a(&mut rv)?;
        let want = <T as MessageTypeId>::MSG_TYPE_ID;
        if mtype == want {
            return Conn::read_a(&mut rv);
        }
        if mtype == 107 {
            let rerror: Rerror = Conn::read_a(&mut rv)?;
            bail!(rerror.ename);
        }
        bail!("unknown type: {}, expected: {}", mtype, want)
    }

    fn read_a<'de, R: Read, T: Deserialize<'de> + Debug>(r: R) -> Result<T> {
        Ok(from_reader(r)?)
    }

    pub fn newfid(&mut self) -> u32 {
        let mut cw = self.writer.lock().unwrap();
        cw.nextfid += 1;
        cw.nextfid
    }
}

const NOFID: u32 = !0;

impl Conn {
    pub fn walk(&mut self, fid: u32, newfid: u32, wname: Vec<String>) -> Result<Vec<Qid>> {
        let (tag, r) = self.new_tag()?;
        let walk = Twalk {
            tag: tag,
            fid,
            newfid,
            wname,
        };
        let rwalk = self.rpc::<Twalk, Rwalk>(&walk, r)?;
        Ok(rwalk.wqid)
    }
    pub fn open(&mut self, fid: u32, mode: OpenMode) -> Result<()> {
        let (tag, r) = self.new_tag()?;
        let open = Topen {
            tag: tag,
            fid,
            mode,
        };
        self.rpc::<Topen, Ropen>(&open, r)?;
        Ok(())
    }
    pub fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>> {
        let (tag, r) = self.new_tag()?;
        let read = Tread {
            tag: tag,
            fid,
            offset,
            count,
        };
        let rread = self.rpc::<Tread, Rread>(&read, r)?;
        Ok(rread.data)
    }
    pub fn write(&mut self, fid: u32, offset: u64, data: Vec<u8>) -> Result<u32> {
        let (tag, r) = self.new_tag()?;
        let write = Twrite {
            tag: tag,
            fid,
            offset,
            data,
        };
        let rwrite = self.rpc::<Twrite, Rwrite>(&write, r)?;
        Ok(rwrite.count)
    }
    pub fn clunk(&mut self, fid: u32) -> Result<()> {
        let (tag, r) = self.new_tag()?;
        let clunk = Tclunk { tag: tag, fid };
        self.rpc::<Tclunk, Rclunk>(&clunk, r)?;
        Ok(())
    }
    pub fn attach(&mut self, user: String, aname: String) -> Result<fsys::Fsys> {
        let newfid = self.newfid();
        let (tag, r) = self.new_tag()?;
        let attach = Tattach {
            tag: tag,
            fid: newfid,
            afid: NOFID,
            uname: user.into(),
            aname: aname.into(),
        };
        let r = self.rpc::<Tattach, Rattach>(&attach, r)?;
        Ok(fsys::Fsys {
            fid: fid::Fid::new(self.clone(), newfid, r.qid),
        })
    }
}
