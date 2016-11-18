use std::io;
use std::str::{from_utf8, Utf8Error};

use netbuf::Buf;
use futures::{Future, Async, Poll};
use futures::Async::{Ready, NotReady};
use futures::stream::{Stream};
use tokio_core::io::Io;
use tk_bufstream::IoBuf;
use byteorder::{BigEndian, ByteOrder};

use super::{Dispatcher, ImmediateReplier, OutFrame};
use self::Frame::*;

const MAX_MESSAGE_SIZE: u64 = 128 << 10;


quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Io(err: io::Error) {
            description("IO error")
            display("IO error: {}", err)
            from()
        }
        InvalidUtf8(err: Utf8Error) {
            description("Error decoding text frame")
            display("Error decoding text frame: {}", err)
            from()
        }
        InvalidOpcode(code: u8) {
            description("Opcode of the frame is invalid")
            display("Opcode of the frame is invalid: {}", code)
            from()
        }
        Unmasked {
            description("Received unmasked frame")
        }
        Fragmented {
            description("Received fragmented frame")
        }
        TooLong {
            description("Received frame that is too long")
        }
    }
}


pub struct WebsockProto<S: Io, D: Dispatcher, R> {
    dispatcher: D,
    io: IoBuf<S>,
    recv: R,
}

pub enum Frame<'a> {
    Ping(&'a [u8]),
    Pong(&'a [u8]),
    Text(&'a str),
    Binary(&'a [u8]),
}

fn parse_frame<'x>(buf: &'x mut Buf) -> Poll<(Frame<'x>, usize), Error> {
    if buf.len() < 2 {
        return Ok(NotReady);
    }
    let (size, fsize) = {
        match buf[1] & 0x7F {
            126 => {
                if buf.len() < 4 {
                    return Ok(NotReady);
                }
                (BigEndian::read_u16(&buf[2..4]) as u64, 4)
            }
            127 => {
                if buf.len() < 10 {
                    return Ok(NotReady);
                }
                (BigEndian::read_u64(&buf[2..10]), 10)
            }
            size => (size as u64, 2),
        }
    };
    if size > MAX_MESSAGE_SIZE {
        return Err(Error::TooLong);
    }
    let size = size as usize;
    let start = fsize + 4 /* mask size */;
    if buf.len() < start + size {
        return Ok(NotReady);
    }

    let fin = buf[0] & 0x80 != 0;
    let opcode = buf[0] & 0x0F;
    // TODO(tailhook) should we assert that reserved bits are zero?
    let mask = buf[1] & 0x80 != 0;
    if !fin {
        return Err(Error::Fragmented);
    }
    if !mask {
        return Err(Error::Unmasked);
    }
    let mask = [buf[start-4], buf[start-3], buf[start-2], buf[start-1]];
    for idx in 0..size { // hopefully llvm is smart enough to optimize it
        buf[start + idx] ^= mask[idx % 4];
    }
    let data = &buf[start..(start + size)];
    let frame = match opcode {
        0x9 => Ping(data),
        0xA => Pong(data),
        0x1 => Text(from_utf8(data)?),
        0x2 => Binary(data),
        // TODO(tailhook) implement shutdown packets
        x => return Err(Error::InvalidOpcode(x)),
    };
    return Ok(Ready((frame, start + size)));
}


impl<D, S: Io, R> Future for WebsockProto<S, D, R>
    where D: Dispatcher,
          R: Stream<Item=OutFrame, Error=io::Error>,
{
    type Item = ();
    type Error = Error;
    fn poll(&mut self) -> Poll<(), Error> {
        loop {
            self.poll_recv()?;
            self.io.flush()?;
            let packet_len = if let Ready((frame, bytes)) =
                parse_frame(&mut self.io.in_buf)?
            {
                self.dispatcher.dispatch(frame,
                    &mut ImmediateReplier::new(&mut self.io.out_buf))?;
                Some(bytes)
            } else {
                None
            };
            if let Some(packet_len) = packet_len {
                self.io.in_buf.consume(packet_len);
            } else {
                let nbytes = self.io.read()?;
                if nbytes == 0 {
                    if self.io.done() {
                        return Ok(Async::Ready(()));
                    } else {
                        return Ok(Async::NotReady);
                    }
                } else {
                    continue;
                }
            };
        }
    }
}

impl<S: Io, D, R> WebsockProto<S, D, R>
    where D: Dispatcher,
          R: Stream<Item=OutFrame, Error=io::Error>,
{
    pub fn new(sock: IoBuf<S>, dispatcher: D, remote: R)
        -> WebsockProto<S, D, R>
    {
        WebsockProto {
            io: sock,
            dispatcher: dispatcher,
            recv: remote,
        }
    }

    fn poll_recv(&mut self) -> Result<(), Error> {
        if let Ready(Some(frame)) = self.recv.poll()? {
            let mut imm = ImmediateReplier::new(&mut self.io.out_buf);
            match frame {
                OutFrame::Text(val) => {
                    imm.text(&val);
                }
                OutFrame::Binary(val) => {
                    imm.binary(&val);
                }
            }
        }
        Ok(())
    }
}
