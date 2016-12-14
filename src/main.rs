#[macro_use]
extern crate log;

extern crate clap;
extern crate pretty_env_logger;
extern crate rustyline;

use rustyline::completion::FilenameCompleter;
use rustyline::error::ReadlineError;

mod btclient;

use btclient::BTClient;

const HISTORY_FILE: &'static str = ".rustyline_history";

#[allow(unknown_lints)]
#[allow(cyclomatic_complexity)]
fn main() {
    pretty_env_logger::init();

    let matches = clap::App::new("btclient")
        .version("0.1.0")
        .arg(clap::Arg::with_name("port").required(true).index(1).help("e.g.: 6881"))
        .get_matches();

    let port = matches.value_of("port").unwrap().parse::<u16>().unwrap();
    info!("port: {}", port);

    let mut btclient = BTClient::new(port);

    let config = rustyline::Config::builder()
        .history_ignore_space(true)
        .completion_type(rustyline::CompletionType::List)
        .build();
    let c = FilenameCompleter::new();
    let mut rl = rustyline::Editor::with_config(config);
    rl.set_completer(Some(c));

    if rl.load_history(HISTORY_FILE).is_err() {
        info!("No previous history!");
    }

    loop {
        let readline = rl.readline("> ");
        match readline {
            Ok(line) => {
                rl.add_history_entry(line.as_ref());

                let line = line.trim().split(' ').collect::<Vec<&str>>();
                match line[0] {
                    "help" | "h" => {
                        println!("Commands:
add/a <torrent file path>        - add a new torrent file for tracking
remove/r <torrent id>            - remove given torrent from tracking list
list/l                           - list torrents being tracked
showfiles/sf <torrent id>        - show files in the torrent
download/d <torrent id>          - download this torrent
seed/s <torrent id>              - seed this torrent
help/h                           - show this help");
                    }
                    "add" | "a" => {
                        if line.len() != 2 {
                            error!("usage: add <torrent file>");
                        } else if let Ok(file) = std::fs::File::open(line[1]) {
                            btclient.add(file).unwrap();
                        } else {
                            error!("Unable to open file, are you sure it exists?");
                        }
                    }
                    "remove" | "r" => {
                        if line.len() != 2 {
                            error!("usage: remove <torrent number>");
                        } else {
                            let id = line[1].parse::<usize>().unwrap();
                            btclient.remove(id).unwrap();
                        }
                    }
                    "list" | "l" => {
                        if line.len() != 1 {
                            error!("usage: list");
                        } else {
                            let t_list = btclient.list();
                            println!("ID  Torrent");
                            println!("--  ----------------------------------------------");
                            for t in t_list {
                                println!("{:2}  {}", t.0, t.1);
                            }
                        }
                    }
                    "showfiles" | "sf" => {
                        if line.len() != 2 {
                            error!("usage: showfiles <torrent id>");
                        } else {
                            // TODO call a btclient fn
                        }
                    }
                    "download" | "d" => {
                        if line.len() != 2 {
                            error!("usage: download <torrent id>");
                        } else {
                            let id = line[1].parse::<usize>().unwrap();
                            btclient.start_download(id);
                        }
                    }
                    "seed" | "s" => {
                        if line.len() != 2 {
                            error!("usage: seed <torrent id>");
                        } else {
                            // TODO call a btclient fn
                        }
                    }
                    "" => {}
                    _ => {
                        println!("invalid command, see \"help\"");
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                info!("CTRL-C");
                break;
            }
            Err(ReadlineError::Eof) => {
                info!("CTRL-D");
                break;
            }
            Err(err) => {
                error!("Error: {:?}", err);
                break;
            }
        }
    }
    rl.save_history(HISTORY_FILE).unwrap();
}
