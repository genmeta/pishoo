use std::{hint::black_box, io::Write, time::Duration};

use rusqlite::{Connection, OpenFlags};
use wasip2::http::types::{
    Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};

struct Probe;

fn database() -> rusqlite::Result<Connection> {
    let db = Connection::open_with_flags_and_vfs(
        "/db/data.sqlite",
        OpenFlags::SQLITE_OPEN_READ_WRITE,
        "unix-dotfile",
    )?;
    db.busy_timeout(Duration::from_millis(100))?;
    Ok(db)
}

fn spill(db: &Connection) {
    db.execute_batch(
        "PRAGMA synchronous = FULL;
         PRAGMA cache_size = 8;
         BEGIN IMMEDIATE;
         UPDATE records SET body = 'uncommitted' WHERE id = 1;
         WITH RECURSIVE test(id) AS (
             SELECT 2 UNION ALL SELECT id + 1 FROM test WHERE id < 100
         )
         INSERT INTO records SELECT id, zeroblob(4096) FROM test;",
    )
    .unwrap();
}

fn respond(out: ResponseOutparam, status: u16, text: &str) {
    let response = OutgoingResponse::new(Fields::new());
    response.set_status_code(status).unwrap();
    let body = response.body().unwrap();
    ResponseOutparam::set(out, Ok(response));
    body.write().unwrap().write_all(text.as_bytes()).unwrap();
    OutgoingBody::finish(body, None).unwrap();
}

impl wasip2::exports::http::incoming_handler::Guest for Probe {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        match request.path_with_query().as_deref() {
            Some("/early") => {
                let db = database().unwrap();
                spill(&db);
                std::fs::create_dir("/db/ordinary.lock").unwrap();
                loop {
                    black_box(0);
                }
            }
            Some("/cancel") => {
                let db = database().unwrap();
                spill(&db);
                let response = OutgoingResponse::new(Fields::new());
                let body = response.body().unwrap();
                ResponseOutparam::set(out, Ok(response));
                wasip2::clocks::monotonic_clock::subscribe_duration(60_000_000_000).block();
                OutgoingBody::finish(body, None).unwrap();
            }
            _ => {
                let record = database().and_then(|db| {
                    db.query_row("SELECT count(*), body FROM records", [], |row| {
                        Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
                    })
                });
                match record {
                    Ok((count, body)) => respond(out, 200, &format!("{count}:{body}")),
                    Err(error) => respond(out, 503, &error.to_string()),
                }
            }
        }
    }
}

impl wasip2::exports::cli::run::Guest for Probe {
    fn run() -> Result<(), ()> {
        let db = database().unwrap();
        spill(&db);
        std::arch::wasm32::unreachable();
    }
}

wasip2::http::proxy::export!(Probe);
wasip2::cli::command::export!(Probe);
