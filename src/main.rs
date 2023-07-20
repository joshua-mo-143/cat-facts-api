use anyhow::anyhow;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::prelude::*;
use libsql_client::{client::Client, Statement};
use reqwest::Client as ReqwestClient;
use serde::{Deserialize, Serialize};
use shuttle_secrets::SecretStore;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

#[derive(Deserialize, Serialize)]
pub struct CatFact {
    fact: String,
}

pub struct CustomService {
    db: Arc<Mutex<Client>>,
    mg_key: String,
    mg_url: String,
}

pub struct AppState {
    db: Arc<Mutex<Client>>,
    mg_key: String,
    mg_url: String,
    ctx: ReqwestClient,
}

pub struct EmailRequest {
    email: String,
}

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "It works!".to_string())
}

#[shuttle_runtime::main]
async fn axum(
    #[shuttle_secrets::Secrets] store: SecretStore,
    #[shuttle_turso::Turso(addr = "{secrets.TURSO_ADDR}", token = "{secrets.TURSO_TOKEN}")]
    db: Client,
) -> Result<CustomService, shuttle_runtime::Error> {
    let mg_key = store
        .get("MAILGUN_KEY")
        .unwrap_or_else(|| "None".to_string());
    let mg_url = store
        .get("MAILGUN_URL")
        .unwrap_or_else(|| "None".to_string());

    Ok(CustomService {
        db: Arc::new(Mutex::new(db)),
        mg_key,
        mg_url,
    })
}

#[shuttle_runtime::async_trait]
impl shuttle_runtime::Service for CustomService {
    async fn bind(mut self, addr: std::net::SocketAddr) -> Result<(), shuttle_runtime::Error> {
        let ctx = ReqwestClient::new();

        self.db
            .lock()
            .await
            .execute(
                "CREATE TABLE IF NOT EXISTS catfacts (
        id integer primary key autoincrement,
        fact text not null,
        created_at datetime default current_timestamp 
        );
            CREATE TABLE IF NOT EXISTS subscribers (
                    id integer primary key autoincrement,
                    email text not null,
        created_at datetime default current_timestamp 
                )",
            )
            .await
            .unwrap();

        let state = Arc::new(AppState {
            db: self.db.clone(),
            mg_key: self.mg_key.to_owned(),
            mg_url: self.mg_url.to_owned(),
            ctx,
        });

        let router = Router::new()
            .route("/", get(health_check))
            .route("/catfact", get(get_record))
            .route("/catfact/create", post(create_record))
            .with_state(state);

        let router = axum::Server::bind(&addr).serve(router.into_make_service());

        tokio::select!(
            _ = router => {},
            _ = send_mail(self.db, self.mg_key, self.mg_url) => {}
        );

        Ok(())
    }
}

pub async fn get_record(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, impl IntoResponse> {
    let res = match state
        .db
        .lock()
        .await
        .execute("SELECT fact FROM catfacts order by random() limit 1")
        .await
    {
        Ok(res) => res,
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    };

    let res = CatFact {
        fact: res.rows[0].values[0].to_string(),
    };

    Ok((StatusCode::OK, Json(res)))
}

pub async fn create_record(
    State(state): State<Arc<AppState>>,
    Json(json): Json<CatFact>,
) -> Result<impl IntoResponse, impl IntoResponse> {
    if let Err(e) = state
        .db
        .lock()
        .await
        .execute(Statement::with_args(
            "INSERT into CATFACTS (fact) VALUES (?)",
            &[json.fact],
        ))
        .await
    {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    }

    Ok(StatusCode::CREATED)
}

pub async fn subscribe(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EmailRequest>,
) -> Result<impl IntoResponse, impl IntoResponse> {
    let api_endpoint = format!(
        "https://api.mailgun.net/v3/lists/mail@{}/members",
        &state.mg_url
    );

    let params = sub_params(req.email.to_owned());
    let post = state
        .ctx
        .post(api_endpoint)
        .basic_auth("api", Some(&state.mg_key))
        .form(&params);

    let _ = match post.send().await {
        Ok(_) => Ok(StatusCode::OK),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    if let Err(e) = state
        .db
        .lock()
        .await
        .execute(Statement::with_args(
            "INSERT INTO subscribers (email) VALUE (?)",
            &[req.email],
        ))
        .await
    {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    };

    Ok(StatusCode::CREATED)
}

fn send_mail_params(recipient: String, fact: String) -> HashMap<&'static str, String> {
    let mut params = HashMap::new();

    params.insert("to", recipient);
    params.insert("subject", "Your daily cat fact!".to_string());
    params.insert("text", format!("Hey there! You're receiving an email because you're subscribed to Cat Facts, the number one source for facts about facts. \n\n Did you know? {fact}"));

    params
}

fn sub_params(recipient: String) -> HashMap<&'static str, String> {
    let mut params = HashMap::new();

    params.insert("address", recipient);
    params.insert("subscribed", "True".to_string());

    params
}

#[allow(unreachable_code)]
pub async fn send_mail(
    db: Arc<Mutex<Client>>,
    mg_key: String,
    mg_url: String,
) -> Result<(), anyhow::Error> {
    let ctx = ReqwestClient::new();

    loop {
        let time = Utc::now().format("%H:%M:%S").to_string();
        println!("The time is: {time}");

        if time.as_str() == "00:00:00" {
            let db = db.lock().await;

            let cat_fact = match db
                .execute("SELECT fact FROM catfacts order by random() limit 1")
                .await
            {
                Ok(res) => res.rows[0].values[0].to_string(),
                Err(e) => return Err(anyhow!("Error when trying to get a cat fact: {e}")),
            };
            let rows = match db.execute("SELECT email FROM subscribers").await {
                Ok(res) => res.rows,
                Err(e) => return Err(anyhow!("Had an error while sending emails: {e}")),
            };

            for row in rows {
                let api_endpoint = format!("https://api.mailgun.net/v3/mail@{}/messages", mg_url);

                let params = send_mail_params(row.values[0].to_string(), cat_fact.clone());

                let post = ctx
                    .post(api_endpoint)
                    .basic_auth("api", Some(mg_key.clone()))
                    .form(&params);

                if let Err(e) = post.send().await {
                    println!("Error while sending subscriber email: {e}")
                };
            }
        }

        let _ = sleep(Duration::from_secs(1)).await;
    }

    Ok(())
}
