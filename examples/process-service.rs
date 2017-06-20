extern crate byteorder;
extern crate bytes;
extern crate futures;
extern crate tokio_core;
extern crate tokio_service;
extern crate tokio_process;
extern crate tokio_proto;
extern crate tokio_io;

use std::sync::Arc;
use std::process::{Command, Stdio};
use std::io;
use std::io::{Read, Write};
use std::str;

use byteorder::BigEndian;
use bytes::{BytesMut, IntoBuf, Buf, BufMut};
use futures::{Future, Poll, Async, Stream, stream};
use tokio_core::reactor::{Core, Handle};
use tokio_process::{CommandExt, Child, ChildStdin, ChildStdout};
use tokio_proto::multiplex::{ClientProto, Multiplex, RequestId};
use tokio_proto::{BindClient};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_io::codec::{Decoder, Encoder, Framed};
use tokio_service::Service;

struct LineCodec;

impl Decoder for LineCodec {
    type Item = (RequestId, String);
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut)
             -> io::Result<Option<(RequestId, String)>>
    {
        // At least 5 bytes are required for a frame: 4 byte
        // head + one byte '\n'
        if buf.len() < 5 {
            // We don't yet have a full message
            return Ok(None);
        }

        // Check to see if the frame contains a new line, skipping
        // the first 4 bytes which is the request ID
        let newline = buf[4..].iter().position(|b| *b == b'\n');
        if let Some(n) = newline {
            // remove the serialized frame from the buffer.
            let mut line = buf.split_to(n + 4);

            // Also remove the '\n'
            buf.split_to(1);

            // Deserialize the request ID
            let id = line.split_to(4).into_buf().get_u32::<BigEndian>();

            // Turn this data into a UTF string and return it in a Frame.
            return match str::from_utf8(&line[..]) {
                Ok(s) => Ok(Some((id as RequestId, s.to_string()))),
                Err(_) => Err(io::Error::new(io::ErrorKind::Other,
                                             "invalid string")),
            }
        }

        // No `\n` found, so we don't have a complete message
        Ok(None)
    }
}

impl Encoder for LineCodec {
    type Item = (RequestId, String);
    type Error = io::Error;

    fn encode(&mut self,
              msg: (RequestId, String),
              buf: &mut BytesMut) -> io::Result<()>
    {
        let (id, msg) = msg;

        buf.put_u32::<BigEndian>(id as u32);
        buf.put(msg.as_bytes());
        buf.put("\n");

        Ok(())
    }
}

struct SedClient;

impl<T: AsyncRead + AsyncWrite + 'static> ClientProto<T> for SedClient {
    type Request = String;
    type Response = String;

    type Transport = Framed<T, LineCodec>;
    type BindTransport = Result<Self::Transport, io::Error>;

    fn bind_transport(&self, io: T) -> Self::BindTransport {
        Ok(io.framed(LineCodec))
    }
}


#[derive(Debug)]
struct ProcessClient<P> {
    proto: Arc<P>,
    handle: Handle,
    command: Command,
}

impl<'a, P> ProcessClient<P>
    where P: BindClient<Multiplex, ChildIO< 'a>> {
    pub fn new(protocol: P, mut command: Command, handle: Handle) -> ProcessClient<P> {
        command.stdin(Stdio::piped())
            .stdout(Stdio::piped());
        ProcessClient {
            proto: Arc::new(protocol),
            command: command,
            handle: handle,
        }
    }
}

struct ChildIO<'a> {
    stdin: Option<&'a mut ChildStdin>,
    stdout: Option<&'a mut ChildStdout>,
}

impl<'a> From<Child> for ChildIO<'a> {
    fn from(child: Child) -> Self {
        ChildIO {
            stdin: child.stdin().as_ref(),
            stdout: child.stdout().as_ref(),
        }
    }
}

impl<'a> Write for ChildIO<'a> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(ref mut stdin) = self.stdin {
            stdin.write(bytes)
        } else {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdin is not open"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(ref mut stdin) = self.stdin {
            stdin.flush()
        } else {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdin is not open"))
        }
    }
}

impl<'a> AsyncWrite for ChildIO<'a> {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        if let Some(ref mut stdin) = self.stdin {
            stdin.shutdown()
        } else {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdin is not open"))
        }
    }
}

impl<'a> Read for ChildIO<'a> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if let Some(ref mut stdout) = self.stdout {
            stdout.read(bytes)
        } else {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdout is not open"))
        }
    }
}

impl<'a> AsyncRead for ChildIO<'a> {
}


impl<'a, P> Future for ProcessClient<P>
    where P: BindClient<Multiplex, ChildIO<'a>> {
    type Item = P::BindClient;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<P::BindClient, io::Error> {
        let child = self.command.spawn_async(&self.handle)?;
        let childio: ChildIO = child.into();
        Ok(Async::Ready(self.proto.bind_client(&self.handle, childio)))
    }
}


fn main() {
    let mut core = Core::new().unwrap();
    let mut command = Command::new("sed");
    command.arg("-u")
        .arg("-e")
        .arg("s/please/pretty please/");

    let child_service = ProcessClient::new(SedClient, command, core.handle());

    let work = child_service.and_then(|service| {
         stream::futures_unordered(vec![
             service.call(String::from("please alter this string for me")),
             service.call(String::from("please alter this string too"))
         ]).collect()
    });

    println!("output: {:?}", core.run(work));
}
