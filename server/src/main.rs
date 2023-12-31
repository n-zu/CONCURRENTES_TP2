mod server;
mod threadpool;

use points::parse_addr;
use server::Server;
use tracing::{error, Level};
use tracing_subscriber::FmtSubscriber;

fn parse_args() -> Result<(String, Option<String>), ()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 {
        return Ok((parse_addr(args[1].clone()), None));
    }
    if args.len() == 3 {
        return Ok((
            parse_addr(args[1].clone()),
            Some(parse_addr(args[2].clone())),
        ));
    }
    error!("Usage: local_server <address> [<known_server_address>]");
    Err(())
}

fn init_logger() {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::DEBUG)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
}

fn main() {
    init_logger();

    if let Ok((addr, core_server_addr)) = parse_args() {
        let server = Server::new(addr, core_server_addr);
        let handler = server.listen();

        handler.join().unwrap();
    }
}

#[cfg(test)]
mod tests {
    //use super::*;

    #[test]
    fn test_hello_world() {
        assert_eq!(1, 1);
    }
}
