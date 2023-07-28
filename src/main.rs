use anyhow::anyhow;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use lettre::{
    message::header::ContentType, transport::smtp::authentication::Credentials, AsyncSmtpTransport,
    AsyncTransport, Message, Tokio1Executor,
};
use libsql_client::{client::Client, Statement};
use serde::{Deserialize, Serialize};
use shuttle_secrets::SecretStore;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration as TokioDuration};
use chrono::Local;
use chrono::naive::{NaiveDateTime, Days};
use std::time::Duration;

#[derive(Deserialize, Serialize)]
pub struct CatFact {
    fact: String,
}

pub struct CustomService {
    db: Arc<Mutex<Client>>,
    gmail_user: String,
    gmail_password: String,
    router: Router
}

pub struct AppState {
    db: Arc<Mutex<Client>>,
}

#[derive(Deserialize)]
pub struct EmailRequest {
    email: String,
}

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "It works!".to_string())
}

async fn homepage() -> impl IntoResponse {
    r#"Welcome to the Cat Facts API!

Here are the following routes:
    - GET /health - Health check route.
    - GET /catfact - Get a random cat fact.
    - POST /catfact/create - Submit your own cat fact
        - Takes the following JSON parameters: "fact"
    - POST /subscribe - Subscribe to our free daily cat fact email service
        - Takes the following JSON parameters: "email"
"#
}

#[shuttle_runtime::main]
async fn axum(
    #[shuttle_secrets::Secrets] store: SecretStore,
    #[shuttle_turso::Turso(addr = "{secrets.TURSO_ADDR}", token = "{secrets.TURSO_TOKEN}")]
    db: Client,
) -> Result<CustomService, shuttle_runtime::Error> {
    let gmail_user = store
        .get("GMAIL_USER")
        .unwrap_or_else(|| "None".to_string());
    let gmail_password = store
        .get("GMAIL_PASSWORD")
        .unwrap_or_else(|| "None".to_string());

    
        db.batch([
                "CREATE TABLE IF NOT EXISTS catfacts (
        id integer primary key autoincrement,
        fact text not null,
        created_at datetime default current_timestamp 
        )",
                "CREATE TABLE IF NOT EXISTS subscribers (
                    id integer primary key autoincrement,
                    email text not null,
        created_at datetime default current_timestamp 
                )",
            ])
            .await
            .unwrap();


        let db = Arc::new(Mutex::new(db));
    
        let state = Arc::new(AppState {
            db: db.clone(),
        });

        let router = Router::new()
            .route("/", get(homepage))
            .route("/health", get(health_check))
            .route("/catfact", get(get_record))
            .route("/catfact/create", post(create_record))
            .route("/subscribe", post(subscribe))
            .with_state(state);


    Ok(CustomService {
        db,
        gmail_user,
        gmail_password,
        router
    })
}

#[shuttle_runtime::async_trait]
impl shuttle_runtime::Service for CustomService {
    async fn bind(mut self, addr: std::net::SocketAddr) -> Result<(), shuttle_runtime::Error> {
        let router = axum::Server::bind(&addr).serve(self.router.into_make_service());

        tokio::select!(
            _ = router => {},
            _ = scheduled_tasks(self.db, self.gmail_user, self.gmail_password) => {}
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
    match state
        .db
        .lock()
        .await
        .execute(Statement::with_args(
            "INSERT into CATFACTS (fact) VALUES (?)",
            &[json.fact],
        ))
        .await
    {
        Ok(_) => Ok((StatusCode::CREATED, "Fact created!".to_string())),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
    }
}

pub async fn subscribe(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EmailRequest>,
) -> Result<impl IntoResponse, impl IntoResponse> {
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

    Ok((StatusCode::CREATED, "You're now subscribed!".to_string()))
}

#[allow(unreachable_code)]
pub async fn scheduled_tasks(
    db: Arc<Mutex<Client>>,
    gmail_user: String,
    gmail_password: String,
) -> Result<(), anyhow::Error> {
    let creds = Credentials::new(gmail_user.to_owned(), gmail_password.to_owned());

    // Open a remote connection to gmail
    let mailer: AsyncSmtpTransport<Tokio1Executor> =
        AsyncSmtpTransport::<Tokio1Executor>::relay("smtp.gmail.com")
            .unwrap()
            .credentials(creds)
            .build();

    
    let mut tomorrow_midnight = Local::now().checked_add_days(Days::new(1)).unwrap().date_naive().and_hms_opt(0, 0, 0).unwrap();

    loop {
        let duration = calculate_time_diff(tomorrow_midnight);
        
        if duration == std::time::Duration::ZERO {

        send_subscriber_mail(mailer.to_owned(), db.clone()).await.expect("Looks like something went wrong trying to send subscriber mail :(");
        
    tomorrow_midnight = Local::now().checked_add_days(Days::new(1)).unwrap().date_naive().and_hms_opt(0, 0, 0).unwrap();
        }
        let duration = calculate_time_diff(tomorrow_midnight);
        
        sleep(TokioDuration::from_secs(duration.as_secs())).await;
    }

    Ok(())
}

fn calculate_time_diff(midnight: NaiveDateTime) -> Duration {
    let now = Local::now().naive_local();

        midnight
            .signed_duration_since(now)
            .to_std()
            .unwrap()
}

async fn send_subscriber_mail(
    mailer: AsyncSmtpTransport<Tokio1Executor>,
    db: Arc<Mutex<Client>>,
) -> Result<(), anyhow::Error> {
        let db = db.lock().await;

        let cat_fact = match db
            .execute("SELECT fact FROM catfacts order by random() limit 1")
            .await
        {
            Ok(res) => res.rows[0].values[0].to_string(),
            Err(e) => return Err(anyhow!("error when trying to get a cat fact: {e}")),
        };

        let rows = match db.execute("SELECT email FROM subscribers").await {
            Ok(res) => res.rows,
            Err(e) => return Err(anyhow!("Had an error while sending emails: {e}")),
        };

        if rows.len() > 0 {
        for row in rows {
            let email = Message::builder()

                    .from("Cat Facts".parse().unwrap())
                    .to(row.values[0].to_string().parse().unwrap())
                    .subject("Happy new year")
                    .header(ContentType::TEXT_PLAIN)
                    .body(format!("Hey there! You're receiving this message because you're subscribed to Cat Facts. \n\nDid you know {cat_fact}?"))
                    .unwrap();

            if let Err(e) = mailer.send(email).await {
                    println!("Something went wrong while sending mail: {e}")
                }
            }
    }

    Ok(())
}