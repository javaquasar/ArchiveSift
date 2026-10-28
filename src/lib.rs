pub mod app;
pub mod clipboard_listener;
pub mod db;
pub mod error;
pub mod fs;
pub mod http;
pub mod logging;
pub mod models;
pub mod persistence;
pub mod scraper;
pub mod services;
pub mod ui;

pub type AppResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
