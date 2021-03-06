#![recursion_limit="1024"]
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::pin::Pin;
use std::sync::Arc;

use clap::{
    crate_version, crate_authors,
    App, Arg, ArgGroup,
};

use bytes::Bytes;

use futures::{
    stream::{
        StreamExt,
    },
};
use pin_utils::pin_mut;

use rml_rtmp::{
    sessions::StreamMetadata,
    time::RtmpTimestamp,
};
use slog::{info, warn};

mod error;
mod rtmp;
mod flv;
mod logger;
mod rtmp_url;
use rtmp_url::Url;


const USAGE: &str = "
    waterfall [FLAGS] [OPTIONS] --input <INPUT> <DEST_LIST_FILE>
    waterfall [FLAGS] [OPTIONS] --input <INPUT> --concurrency <CONCURRENCY> --prefix <PREFIX>";

const EXAMPLE: &str = "
EXAMPLES:

    ## Auto-Generated destinations

    > waterfall --input test.flv -c 100 -p rtmp://test.example.com/app/stream_prefix_

    This command will read from test.flv, push RTMP stream to the following destinations concurrently:

        rtmp:://test.example.com/app/stream_prefix_0
        rtmp:://test.example.com/app/stream_prefix_1
        rtmp:://test.example.com/app/stream_prefix_2
        ...
        rtmp:://test.example.com/app/stream_prefix_99

    ## From destinations list file

    > waterfall --input test.flv target_list.txt

    This command will read from test.flv, push RTMP stream to the destinations read from `target_list.txt` concurrently:

    > cat target_list.txt

        rtmp:://test.example.com/app/stream_LIho834J
        rtmp:://test.example.com/app/stream_HliH234L
        rtmp:://test.example.com/app/stream_AhBhi33j
        ...
        rtmp:://test.example.com/app/stream_Eie83lrF
";

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let (root_logger, _guard) = logger::init();
    #[allow(deprecated)]
    let matches = App::new("RTMP Publish Bench Tool")
        .version(crate_version!())
        .author(crate_authors!("\n"))
        .about("This tool read flv packages from a specified file and push it to destinations from list or generated path, concurrently.")
        .usage(USAGE)
        .after_help(EXAMPLE)
        .arg(Arg::with_name("INPUT")
            .short("i")
            .long("input")
            .help("Input FLV file path")
            .required(true)
            .takes_value(true))

        .arg(Arg::with_name("repeat")
            .short("r")
            .long("repeat"))

        .arg(Arg::with_name("CONCURRENCY")
            .short("c")
            .long("concurrency")
            .takes_value(true))
        .arg(Arg::with_name("PREFIX")
            .short("p")
            .long("prefix")
            .help("RTMP destinations prefix, e.g. `rtmp://example.com/app/stream_`")
            .takes_value(true))

        .arg(Arg::with_name("DEST_LIST_FILE")
             .help("Sets the input file to use")
             .index(1))

        .group(ArgGroup::with_name("prefix group")
            .args(&["PREFIX"])
            .conflicts_with("DEST_LIST_FILE")
            .requires("CONCURRENCY"))
        .group(ArgGroup::with_name("list group")
            .arg("DEST_LIST_FILE")
            .conflicts_with_all(&["prefix group", "CONCURRENCY"]))
        .get_matches();

    let urls: Box<dyn Iterator<Item = String>> = if matches.is_present("PREFIX") {
        let concurrency = matches.value_of("CONCURRENCY").map(|c| {
            c.parse::<usize>().expect("Cannot parse `CONCURRENCY`")
        }).unwrap_or(1);
        let prefix = matches.value_of("PREFIX").unwrap();
        let urls = (0..concurrency).map(move |c| format!("{}{}", prefix, c));
        Box::new(urls)
    } else {
        // Read from list file
        let dest_file_path = matches.value_of("DEST_LIST_FILE").unwrap();
        let list_file = File::open(dest_file_path)?;
        let reader = BufReader::new(list_file);
        let urls = reader.lines().map(|r| r.unwrap());
        Box::new(urls)
    };
    let urls = urls.map(|u| rtmp_url::parse_rtmp_url(u.as_str())).collect::<Vec<Result<Url, _>>>();
    let repeat = matches.is_present("repeat");

    if let Some(Err(e)) = urls.iter().find(|u| u.is_err()) {
        panic!("RTMP url error: {}", e);
    }

    let urls = urls.into_iter().map(|r| r.unwrap()).collect::<Vec<Url>>();

    let input_file_path = matches.value_of("INPUT").unwrap();
    assert!(input_file_path.ends_with(".flv") || input_file_path.ends_with(".FLV"),
        "Only FLV files are supported");
    let msgs = flv::read_flv_tag(input_file_path, repeat, root_logger.clone()).await?;

    let (tx, _rx) = tokio::sync::broadcast::channel(1024);

    let clients = futures::stream::futures_unordered::FuturesUnordered::new();
    for url in urls {
        let rx = tx.subscribe();
        let client_fut = rtmp::client::Client::new(url, rx, &root_logger);
        clients.push(client_fut);
    }

    pin_mut!(msgs);
    let mut msgs: Pin<&mut _> = msgs;

    // await for all publish client ready
    let _ = clients.collect::<Vec<_>>().await;
    info!(root_logger, "All publish clients are ready");

    // broadcast
    while let Some(Ok(msg)) = msgs.next().await {
        if tx.receiver_count() <= 0 {
            warn!(root_logger, "No publish client exists, quit");
            break;
        }
        match tx.send(msg) {
            Ok(_num) => { }
            Err(_) => {
                warn!(root_logger, "No publish client exists, quit");
                break;
            }
        }
    }

    info!(root_logger, "End");
    Ok(())
}

#[derive(Clone, Debug)]
pub enum PacketType {
    Metadata(Arc<StreamMetadata>),
    Video {
        data: Bytes,
        ts: RtmpTimestamp,
    },
    Audio {
        data: Bytes,
        ts: RtmpTimestamp,
    },
}

#[derive(Debug)]
pub enum ReceivedType {
    FromClient {
        message: rml_rtmp::messages::MessagePayload,
        bytes_read: usize,
    },
    Broadcast(Arc<PacketType>),
}
