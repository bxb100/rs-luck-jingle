use actix_web::http::header::HeaderValue;
use actix_web::web::Data;
use actix_web::{get, post, web, App, HttpRequest, HttpResponse, HttpServer, Responder};

use chrono::Utc;
use lazy_static::lazy_static;
use regex::Regex;
use rs_luck_jingle::printer::{call_printer, init_printer};
use serde::Deserialize;
use tokio::sync::mpsc::{channel, Sender};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let (tx, mut rx) = channel::<String>(100);

    let shared_sender = Data::new(tx);

    tokio::spawn(async move {
        let (printer, cmd) = init_printer().await.unwrap();
        while let Some(s) = rx.recv().await {
            let _ = call_printer(s.as_str(), &printer, &cmd).await;
        }
    });

    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    HttpServer::new(move || {
        App::new()
            .app_data(shared_sender.clone())
            .service(index)
            .service(github_webhooks)
    })
    .bind(("127.0.0.1", 5444))?
    .run()
    .await
}

#[get("/")]
async fn index() -> impl Responder {
    "Ok"
}

lazy_static! {
    static ref DEFAULT_HEADER: HeaderValue = HeaderValue::from_static("none");
    static ref LINK_REGEX: Regex = Regex::new(r"!?\[.*?]\(.*?\)").unwrap();
}
#[post("/github-webhooks")]
async fn github_webhooks(
    sender: Data<Sender<String>>,
    hook: web::Json<GithubWebhook>,
    req: HttpRequest,
) -> impl Responder {
    let github_event = req
        .headers()
        .get("X-GitHub-Event")
        .unwrap_or(&DEFAULT_HEADER)
        .to_str()
        .unwrap();
    log::debug!("hook: {:?}", hook);

    let now = Utc::now().format("%Y-%m-%d %H:%M:%S");
    let str = if github_event == "issues" {
        if hook.0.action.unwrap() != "opened" {
            return HttpResponse::Ok().finish();
        }
        let issue = hook.0.issue.unwrap();
        format!(
            "{}\nREPO: {}\n新的 ISSUE 来了来了来了！\n\
            ISSUE Title: {}\nContent:\n {}",
            now,
            hook.0.repository.full_name,
            issue.title,
            truncate(
                LINK_REGEX
                    .replace_all(issue.body.unwrap_or("".to_string()).trim(), "")
                    .as_ref(),
                60,
            )
        )
    } else if github_event == "issue_comment" {
        if hook.0.action.unwrap() != "created" {
            return HttpResponse::Ok().finish();
        }

        format!(
            "{}\nREPO: {}\nISSUE: {}\n{} 刚刚留下了评论",
            now,
            hook.0.repository.full_name,
            hook.0.issue.unwrap().title,
            hook.0.comment.unwrap().user.unwrap().login
        )
    } else if github_event == "ping" {
        format!(
            "{}\nREPO: {}\n{}\n ---- SETUP DONE --- ",
            now,
            hook.0.repository.full_name,
            hook.0.zen.unwrap()
        )
    } else {
        log::error!("Unhandled event: {:?}", github_event);
        return HttpResponse::BadRequest().finish();
    };

    if let Err(e) = sender.send(str).await {
        log::error!("channel sender error: {:?}", e)
    }

    HttpResponse::Ok().finish()
}

fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        None => s,
        Some((idx, _)) => &s[..idx],
    }
}

#[derive(Debug, Deserialize)]
struct GithubWebhook {
    zen: Option<String>,
    action: Option<String>,
    issue: Option<Issue>,
    comment: Option<Comment>,
    repository: Repository,
}

#[derive(Debug, Deserialize)]
struct Repository {
    full_name: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct Comment {
    body: String,
    user: Option<User>,
}

#[derive(Debug, Deserialize)]
struct User {
    login: String,
}

#[derive(Debug, Deserialize)]
struct Issue {
    title: String,
    body: Option<String>,
}
