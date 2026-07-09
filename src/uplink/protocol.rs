//! Contrat des messages renvoyés par le serveur d'analyse.
//!
//! Le client envoie des octets bruts (le flux du jeu, non déchiffré) ; le serveur
//! déchiffre, interprète, et répond avec ces messages structurés. Les champs
//! reprennent le contenu d'un item décrit dans la documentation du Secret Shop.

use serde::Deserialize;

/// Message descendant du serveur vers le relais.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Accusé de réception d'un lot d'octets.
    Ack,
    /// Instantané complet du shop décodé par le serveur.
    Shop(ShopSnapshot),
    /// Un ou plusieurs items méritent l'attention du joueur.
    Alert(Alert),
    /// Type inconnu — ignoré (compatibilité ascendante).
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShopSnapshot {
    #[serde(default)]
    pub merchant: Option<String>,
    #[serde(default)]
    pub slots: Vec<ShopItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Alert {
    pub message: String,
    #[serde(default)]
    pub items: Vec<ShopItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShopItem {
    /// Emplacement dans le shop (1 à 6). `0` si le serveur l'omet.
    #[serde(default)]
    pub slot: u8,
    /// Type d'item ; `Unknown` plutôt que d'invalider tout le message si absent.
    #[serde(default)]
    pub kind: ItemKind,
    #[serde(default)]
    pub name: Option<String>,
    /// Prix en or (pour le lobby).
    #[serde(default)]
    pub price: Option<u32>,
    /// Grade de l'équipement (2, 3 ou 4).
    #[serde(default)]
    pub grade: Option<u8>,
    /// Set de l'équipement (Vitesse, Coup Critique, …).
    #[serde(default)]
    pub set: Option<String>,
    #[serde(default)]
    pub substats: Vec<SubStat>,
    #[serde(default)]
    pub required_level: Option<u8>,
    #[serde(default)]
    pub limit: Option<PurchaseLimit>,
    /// Verdict du serveur : cet item mérite l'attention.
    #[serde(default)]
    pub interesting: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Equipment,
    Hero,
    Token,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubStat {
    pub name: String,
    #[serde(default)]
    pub value: Option<f64>,
}

/// Limite d'achat, p. ex. « 0/1 » (épuisé) ou « 1/1 » (disponible).
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PurchaseLimit {
    pub remaining: u32,
    pub total: u32,
}
