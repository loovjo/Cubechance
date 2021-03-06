#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate lazy_static;
extern crate actix_web;
extern crate chrono;
extern crate fuzz_search;
extern crate percent_encoding;

use chrono::Local;
use std::env::args;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use actix_web::{http::Method, server, App, HttpRequest, HttpResponse, Responder};

use percent_encoding::percent_decode;

mod wca;
use wca::wca_export;

const COMP_SPEED_MUL: f64 = 0.7376;
const TRAFIC_FILE: &str = "trafic.txt";

lazy_static! {
    // Setup WCA reading in background
    static ref STATE: MutWebState = Arc::new(Mutex::new(WebState::NotLoaded));
}

#[derive(Debug)]
enum WebState {
    Loaded(wca::WcaResults),
    NotLoaded,
    Loading(f64, f64), // (amount_done, dones per second)
}

type MutWebState = Arc<Mutex<WebState>>;

const HTML_NOT_FOUND: &'static str = include_str!("not_found.html");

fn get_trafic_file() -> Result<File, std::io::Error> {
    let path = Path::new(TRAFIC_FILE);

    OpenOptions::new()
        .create(true)
        .read(false)
        .write(true)
        .append(true)
        .truncate(false)
        .open(path)
}

fn write_log(text: &str, req: Option<&HttpRequest>) -> Result<(), std::io::Error> {
    let mut tr = get_trafic_file()?;
    let date = Local::now();
    let to_write = format!(
        "{}{} - {}\n",
        date.format("%F %T"), // TODO: Fix
        req.and_then(|x| x.connection_info().remote().map(|x| format!(" ({})", x)))
            .unwrap_or("".to_string()),
        text
    );

    tr.write(to_write.as_bytes())?;
    Ok(())
}

fn index(req: &HttpRequest) -> impl Responder {
    let path = req.match_info().get("path").unwrap();

    if path.ends_with(".html") {
        // Write to trafic file
        write_log(&format!("Accessing {}", path), Some(req))?;
    }

    read_file(Path::new(path))
}

fn read_file(path: &Path) -> Result<impl Responder, std::io::Error> {
    let path = Path::new("Static").join(path).canonicalize()?;

    if !path.starts_with(Path::new("Static/").canonicalize()?) {
        return Ok(HttpResponse::NotFound().body(HTML_NOT_FOUND.to_string()));
    }

    if let Ok(mut f) = File::open(&path) {
        let fs = f.metadata().map(|x| x.len()).unwrap_or(0);
        let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("txt");

        let mut cnt = Vec::with_capacity(fs as usize);
        f.read_to_end(&mut cnt)?;

        Ok(HttpResponse::Ok().content_type(ext).body(cnt))
    } else {
        Ok(HttpResponse::NotFound().body(HTML_NOT_FOUND.to_string()))
    }
}

fn index_slash(req: &HttpRequest) -> impl Responder {
    write_log("Accessing /", Some(req))?;
    read_file(Path::new("index.html"))
}

// Gets the progress of loading the webstate
fn get_progress(_: &HttpRequest) -> impl Responder {
    let state = STATE.lock().unwrap();

    match *state {
        WebState::Loading(prog, speed) => HttpResponse::Ok().body(format!("{} {}", prog, speed)),
        _ => HttpResponse::InternalServerError().body("not-loaded".to_string()),
    }
}

fn github(req: &HttpRequest) -> Option<impl Responder> {
    let gh_link = req.match_info().get("gh_link")?;
    let file = req.match_info().get("file")?;

    let mut content = reqwest::get(&format!(
        "https://raw.githubusercontent.com/loovjo/{}/master/site/{}",
        gh_link, file
    ))
    .ok()?;

    let mut body = Vec::with_capacity(content.content_length().unwrap_or(0) as usize);
    content.copy_to(&mut body).ok()?;

    Some(HttpResponse::Ok().body(body))
}

fn wca_person<'r>(req: &HttpRequest) -> Option<impl Responder> {
    let name = req.match_info().get("name")?;
    let name = percent_decode(name.as_bytes())
        .decode_utf8()
        .ok()?
        .into_owned();

    let name = name.to_lowercase();

    let state = STATE.lock().unwrap();

    match *state {
        WebState::Loaded(ref wca) => {
            // let people: Vec<_> = wca
            //     .people
            //     .iter()
            //     .filter(|&(id, p)| {
            //         p.name.to_lowercase().contains(&name) || id.to_lowercase().contains(&name)
            //     })
            //     .filter_map(|(id, _)| wca.ext_person(&id))
            //     .take(21)
            //     .collect();
            //
            //
            //
            println!("Searching for {:?}", name);

            let people_vec = wca.people.iter().collect::<Vec<(_, _)>>();

            println!("{:?} people", people_vec.len());

            let best: Vec<_> = fuzz_search::best_matches_scores_key(
                &name,
                people_vec,
                |(_id, p)| p.name.clone(),
                20,
            )
            .map(|x| x.0)
            .collect();

            let best: Vec<_> = best
                .into_iter()
                .map(|(id, _)| wca.ext_person(&id))
                .collect();

            match serde_json::to_string(&best) {
                Ok(json) => Some(HttpResponse::Ok().body(json)),
                _ => None,
            }
        }
        _ => Some(HttpResponse::InternalServerError().body("e0".to_string())),
    }
}

fn wca_id(req: &HttpRequest) -> Option<impl Responder> {
    let id = req.match_info().get("id")?;

    let state = STATE.lock().unwrap();

    match *state {
        WebState::Loaded(ref wca) => {
            let person = wca.ext_person(&id);
            match person {
                Some(person) => match serde_json::to_string(&person) {
                    Ok(json) => Some(HttpResponse::Ok().body(json)),
                    Err(_) => None,
                },
                None => Some(HttpResponse::InternalServerError().body("no person".to_string())),
            }
        }
        _ => Some(HttpResponse::InternalServerError().body("not loaded".to_string())),
    }
}

fn upcoming(_: &HttpRequest) -> Option<impl Responder> {
    let state = STATE.lock().unwrap();

    match *state {
        WebState::Loaded(ref wca) => {
            let comps: Vec<&wca::Competition> =
                wca.comps.values().filter(|comp| !comp.has_been).collect();
            match serde_json::to_string(&comps) {
                Ok(json) => Some(HttpResponse::Ok().body(json)),
                Err(_) => None,
            }
        }
        _ => Some(HttpResponse::InternalServerError().body("not loaded".to_string())),
    }
}

fn beating(req: &HttpRequest) -> Option<impl Responder> {
    let id1 = req.match_info().get("id1")?;
    let id2 = req.match_info().get("id2")?;
    let event = req.match_info().get("event")?;

    let state = STATE.lock().unwrap();

    match *state {
        WebState::Loaded(ref wca) => {
            let p1 = wca.ext_person(&id1);
            let p2 = wca.ext_person(&id2);
            match (p1, p2) {
                (Some(ref p1), Some(ref p2)) => {
                    Some(HttpResponse::Ok().body(format!("{:?}", p1.chance_beating(p2, &event))))
                }
                _ => None,
            }
        }
        _ => Some(HttpResponse::InternalServerError().body("not loaded".to_string())),
    }
}

fn place(req: &HttpRequest) -> Option<impl Responder> {
    let comp = req.match_info().get("comp")?;
    let id = req.match_info().get("id")?;
    let event = req.match_info().get("event")?;

    let state = STATE.lock().unwrap();

    match *state {
        WebState::Loaded(ref wca) => match (wca.ext_person(&id), wca.comps.get(comp)) {
            (Some(ref person), Some(ref comp)) => {
                let competitors: Vec<_> = comp
                    .competitors
                    .iter()
                    .filter(|p| p.id != id && p.events.iter().any(|e| e == &event))
                    .filter_map(|p| wca.ext_person(&p.id))
                    .collect();
                let res = person.place_prob(competitors.as_slice(), &event);
                Some(HttpResponse::Ok().body(format!("{:?}", res)))
            }
            _ => None,
        },
        _ => Some(HttpResponse::InternalServerError().body("not loaded".to_string())),
    }
}

fn comp<'r>(req: &HttpRequest) -> Option<impl Responder> {
    let id = req.match_info().get("id")?;

    let state = STATE.lock().unwrap();

    match *state {
        WebState::Loaded(ref wca) => {
            let comp = wca.comps.values().filter(|comp| comp.id == id).nth(0);

            match comp {
                Some(comp) => match serde_json::to_string(&wca.comp_info(&comp.id)) {
                    Ok(json) => Some(HttpResponse::Ok().body(json)),
                    _ => None,
                },
                None => None,
            }
        }
        _ => Some(HttpResponse::InternalServerError().body("not loaded".to_string())),
    }
}

// #[error(404)]
// fn not_found<'r>(_req: &Request) -> Response<'r> {
//     make_html(HTML_NOT_FOUND.to_string())
// }

fn main() {
    let port = args()
        .nth(1)
        .unwrap_or("8080".into())
        .parse()
        .expect("PORT must be a number");

    let thread_state = STATE.clone();
    thread::spawn(move || {
        loop {
            println!("Downloading wca...");
            let start = Instant::now();
            let mut competitor_start = Instant::now();
            let mut loaded_comps = false;

            let comp = wca_export::download_wca(|prog| {
                let mut state = thread_state.lock().unwrap();
                let prog_f64 = match prog {
                    wca::Progress::LoadedZip => 0.,
                    wca::Progress::LoadedComp(x, y) => x as f64 / y as f64,
                    wca::Progress::StartLoadCompetitor => {
                        competitor_start = Instant::now();
                        loaded_comps = true;
                        0.
                    }
                    wca::Progress::LoadedCompetitor(x, y) => x as f64 / y as f64,
                };

                if !loaded_comps {
                    *state = WebState::Loading(
                        prog_f64 * COMP_SPEED_MUL,
                        COMP_SPEED_MUL * prog_f64 as f64
                            / (Instant::now().duration_since(start)).as_secs() as f64,
                    );
                } else {
                    *state = WebState::Loading(
                        prog_f64 * (1. - COMP_SPEED_MUL) + COMP_SPEED_MUL,
                        (1. - COMP_SPEED_MUL) * prog_f64 as f64
                            / (Instant::now().duration_since(competitor_start)).as_secs() as f64,
                    );
                }
            });

            println!(
                "Downloaded the WCA in {} seconds",
                start.elapsed().as_secs()
            );

            match comp {
                Ok(comp) => {
                    let mut state = thread_state.lock().unwrap();
                    *state = WebState::Loaded(comp);
                }
                Err(e) => {
                    println!("Compressed download failed! Error: {:?}", e);
                }
            }

            thread::sleep(Duration::new(3600 * 24, 0)); // Sleep for 24 hours
        }
    });

    let sys = actix_web::actix::System::new("cubechance");

    server::new(|| {
        App::new()
            .resource("/prog", |r| r.method(Method::GET).f(get_progress))
            .resource("/gh/{gh_link}/{file:.+}", |r| {
                r.method(Method::GET).f(github)
            })
            .resource("/api/people/{name}", |r| {
                r.method(Method::GET).f(wca_person)
            })
            .resource("/api/wca/{id}", |r| r.method(Method::GET).f(wca_id))
            .resource("/api/upcoming", |r| r.method(Method::GET).f(upcoming))
            .resource("/api/beat/{id1}/{id2}/{event}", |r| {
                r.method(Method::GET).f(beating)
            })
            .resource("/api/place/{comp}/{id}/{event}", |r| {
                r.method(Method::GET).f(place)
            })
            .resource("/api/comp/{id}", |r| r.method(Method::GET).f(comp))
            .resource("/", |r| r.method(Method::GET).f(index_slash))
            .resource("/{path:.+}", |r| r.method(Method::GET).f(index))
    })
    .bind(("0.0.0.0", port))
    .unwrap()
    .start();

    println!("Started on {:?}", ("0.0.0.0", port));

    sys.run();
}
