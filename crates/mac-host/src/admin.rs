//! `aether-agent pair`, `clients` and `unpair`: managing who may connect.
//! They work on the same files the daemon reads (see [`crate::trust`]), so
//! they need no running daemon and no channel to it.

use std::collections::HashSet;
use std::time::Duration;

use aetherdesk_proto::DEFAULT_PORT;

use crate::session;
use crate::trust::{PAIRING_TIME, PAIRING_TRIES, Store};

pub const USAGE: &str = "\
usage: aether-agent pair [--serve] [--port N]
       aether-agent clients [--serve]
       aether-agent unpair NAME [--serve]

  pair     open pairing for 2 minutes and show the code to type on the client
  clients  list the paired clients
  unpair   forget a client, by name or the start of its fingerprint
  --serve  the clients of `aether-agent serve`, instead of the installed host
           (which needs root: sudo, or scripts/mac.zsh pair|clients|unpair)
  --port N the port to show in the hint (default 4500)";

struct Opts {
    serve: bool,
    port: u16,
    name: Option<String>,
}

fn parse(args: &[String], wants_name: bool) -> Result<Opts, String> {
    let mut o = Opts {
        serve: false,
        port: DEFAULT_PORT,
        name: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--serve" => o.serve = true,
            "--port" => {
                o.port = it
                    .next()
                    .ok_or("--port needs a value")?
                    .parse()
                    .map_err(|e| format!("--port: {e}"))?
            }
            "-h" | "--help" => return Err(USAGE.into()),
            a if wants_name && o.name.is_none() && !a.starts_with('-') => o.name = Some(a.into()),
            a => return Err(format!("unexpected {a}\n\n{USAGE}")),
        }
    }
    if wants_name && o.name.is_none() {
        return Err(USAGE.into());
    }
    Ok(o)
}

fn store(serve: bool) -> Result<Store, String> {
    if serve {
        return Store::serve();
    }
    if unsafe { libc::geteuid() } != 0 {
        return Err(
            "the installed host's clients are root's: run this with sudo \
             (scripts/mac.zsh pair|clients|unpair), or add --serve for `serve`"
                .into(),
        );
    }
    Ok(Store::system())
}

pub fn pair(args: &[String]) -> Result<(), String> {
    let o = parse(args, false)?;
    let store = store(o.serve)?;
    let id = store
        .identity()
        .map_err(|e| format!("identity in {}: {e}", store.dir().display()))?;
    let before: HashSet<_> = clients(&store)?.iter().map(|c| c.fingerprint).collect();
    let pairing = store.open_pairing().map_err(|e| e.to_string())?;

    let port = if o.port == DEFAULT_PORT {
        String::new()
    } else {
        format!(":{}", o.port)
    };
    println!(
        "Pairing is open for {} minutes. On Linux, run one of:",
        PAIRING_TIME.as_secs() / 60
    );
    for (interface, ip) in session::local_addresses() {
        println!("  aetherdesk pair {ip}{port}    ({interface})");
    }
    println!(
        "and type this code:\n\n    {} {}\n",
        &pairing.code[..3],
        &pairing.code[3..]
    );
    println!("(this host: {})", id.fingerprint().short());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio: {e}"))?;
    runtime.block_on(async {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = tokio::signal::ctrl_c() => {
                    store.close_pairing();
                    return Err("cancelled; pairing is closed".into());
                }
            }
            let now = clients(&store)?;
            if let Some(c) = now.iter().find(|c| !before.contains(&c.fingerprint)) {
                println!("Paired with {} ({}).", c.name, c.fingerprint.short());
                return Ok(());
            }
            if store.pairing().is_none() {
                let tried_out = !store.dir().join("pairing").exists();
                store.close_pairing();
                return Err(if tried_out {
                    format!("{PAIRING_TRIES} wrong codes; pairing is closed")
                } else {
                    "the code expired; pairing is closed".into()
                });
            }
        }
    })
}

pub fn list(args: &[String]) -> Result<(), String> {
    let o = parse(args, false)?;
    let store = store(o.serve)?;
    let all = clients(&store)?;
    if all.is_empty() {
        println!("no paired clients");
    }
    for c in all {
        println!("{}  {}", c.fingerprint.short(), c.name);
    }
    Ok(())
}

pub fn unpair(args: &[String]) -> Result<(), String> {
    let o = parse(args, true)?;
    let store = store(o.serve)?;
    let which = o.name.unwrap_or_default();
    let gone = store.remove_clients(&which).map_err(|e| e.to_string())?;
    if gone.is_empty() {
        return Err(format!(
            "no paired client called {which:?}; `aether-agent clients` lists them"
        ));
    }
    for c in gone {
        println!("unpaired {} ({})", c.name, c.fingerprint.short());
    }
    Ok(())
}

fn clients(store: &Store) -> Result<Vec<crate::trust::Client>, String> {
    store
        .clients()
        .map_err(|e| format!("reading {}: {e}", store.dir().display()))
}
