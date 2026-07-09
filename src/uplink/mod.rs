//! Liaison avec le serveur d'analyse : envoi du flux brut, réception des alertes.

mod websocket;

pub mod protocol;

pub use websocket::WebSocketUplink;
