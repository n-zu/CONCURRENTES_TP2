use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

use super::*;
use actix::prelude::*;
use points::{CLIENT_CONNECTION, MESSAGE_BUFFER_SIZE};

const READ_TIMEOUT: u64 = 1000;

pub struct PointStorage {
    local_server: TcpStream,
}

impl Actor for PointStorage {
    type Context = SyncContext<Self>;
}

impl PointStorage {
    pub fn new(local_server_addr: String) -> Result<Self, String> {
        let mut local_server =
            TcpStream::connect(local_server_addr).or(Err("Could not connect to local server"))?;

        local_server
            .set_read_timeout(Some(Duration::from_millis(READ_TIMEOUT)))
            .map_err(|_| "Could not set read timeout")?;

        local_server
            .write_all(&[CLIENT_CONNECTION])
            .map_err(|_| "Could not write to local server")?;

        Ok(PointStorage { local_server })
    }

    fn write(&mut self, buf: [u8; MESSAGE_BUFFER_SIZE]) -> Result<(), String> {
        self.local_server
            .write_all(&buf)
            .or(Err("Could not write to local server"))?;
        Ok(())
    }

    fn read(&mut self) -> Result<u8, String> {
        let mut buf: [u8; 1] = [0];
        self.local_server
            .read_exact(&mut buf)
            .map_err(|_| "Could not read from local server")?;
        Ok(buf[0])
    }

    fn send(&mut self, msg: PointMessage) -> Result<(), String> {
        self.write(msg.into())?;
        let res = self.read()?;
        if res == 0 {
            Err("Local server returned error".to_string())
        } else {
            Ok(())
        }
    }
}

impl Handler<LockOrder> for PointStorage {
    type Result = Result<(), String>;

    fn handle(&mut self, msg: LockOrder, _ctx: &mut SyncContext<Self>) -> Self::Result {
        let msg = PointMessage::LockOrder(msg.0);
        self.send(msg)
    }
}

impl Handler<FreeOrder> for PointStorage {
    type Result = Result<(), String>;

    fn handle(&mut self, msg: FreeOrder, _ctx: &mut SyncContext<Self>) -> Self::Result {
        let msg = PointMessage::FreeOrder(msg.0);
        self.send(msg)
    }
}

impl Handler<CommitOrder> for PointStorage {
    type Result = Result<(), String>;

    fn handle(&mut self, msg: CommitOrder, _ctx: &mut SyncContext<Self>) -> Self::Result {
        let msg = PointMessage::CommitOrder(msg.0);
        self.send(msg)
    }
}
