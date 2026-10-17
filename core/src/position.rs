//! Position de la caméra sur la moto. Elle fixe la direction « avant » de la vue par défaut
//! (une caméra tournée vers l'arrière regarde la route à 180°) ; le choix est gardé par session,
//! et le dernier choix fait pour une caméra sert de réglage par défaut à ses autres sessions.
//! Fichier data/positions.json : {"sessions": {<session>: <clé>}, "cameras": {<numéro de série>: <clé>}}.

use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::paths;

/// Positions proposées : (clé, libellé, lacet de la vue avant en °).
pub const POSITIONS: [(&str, &str, f64); 5] = [
    ("guidon", "Guidon", 0.0),
    ("casque", "Casque", 0.0),
    ("poitrine", "Poitrine", 0.0),
    ("arriere", "Arrière de la moto", 180.0),
    ("perche", "Perche", 0.0),
];
pub const DEFAULT: &str = "guidon";

pub fn path() -> PathBuf {
    paths::data().join("positions.json")
}

pub fn is_known(key: &str) -> bool {
    POSITIONS.iter().any(|p| p.0 == key)
}

/// Lacet (°) de la vue avant pour une position ; 0 pour une clé inconnue.
pub fn front_yaw(key: &str) -> f64 {
    POSITIONS.iter().find(|p| p.0 == key).map_or(0.0, |p| p.2)
}

/// Liste pour l'interface : [{key, label, yaw}].
pub fn list() -> Value {
    Value::Array(POSITIONS.iter().map(|(key, label, yaw)| json!({"key": key, "label": label, "yaw": yaw})).collect())
}

fn entry<'a>(store: &'a Value, group: &str, id: &str) -> Option<&'a str> {
    store.get(group)?.get(id)?.as_str().filter(|k| is_known(k))
}

/// Position d'une session : son propre choix, sinon celui de sa caméra, sinon le guidon.
pub fn resolve<'a>(store: &'a Value, session: &str, serial: Option<&str>) -> &'a str {
    entry(store, "sessions", session)
        .or_else(|| serial.and_then(|s| entry(store, "cameras", s)))
        .unwrap_or(DEFAULT)
}

/// Enregistre le choix pour la session, et comme réglage par défaut de sa caméra.
pub fn assign(store: &mut Value, session: &str, serial: Option<&str>, key: &str) {
    if !store.is_object() {
        *store = json!({});
    }
    for (group, id) in [("sessions", Some(session)), ("cameras", serial)] {
        let Some(id) = id else { continue };
        let slot = store.as_object_mut().unwrap().entry(group).or_insert_with(|| Value::Object(Map::new()));
        if !slot.is_object() {
            *slot = json!({});
        }
        slot.as_object_mut().unwrap().insert(id.into(), key.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_choice_then_camera_default() {
        let mut store = Value::Null;
        assert_eq!(resolve(&store, "A", Some("S1")), "guidon");
        assign(&mut store, "A", Some("S1"), "arriere");
        assert_eq!(resolve(&store, "A", Some("S1")), "arriere");
        assert_eq!(resolve(&store, "B", Some("S1")), "arriere", "autre session de la même caméra");
        assert_eq!(resolve(&store, "B", Some("S2")), "guidon", "autre caméra");
        assign(&mut store, "B", Some("S1"), "casque");
        assert_eq!((resolve(&store, "A", Some("S1")), resolve(&store, "C", Some("S1"))), ("arriere", "casque"));
        assert_eq!((front_yaw("arriere"), front_yaw("casque"), front_yaw("inconnue")), (180.0, 0.0, 0.0));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let store = json!({"sessions": {"A": "plafond"}, "cameras": {"S1": 3}});
        assert_eq!(resolve(&store, "A", Some("S1")), "guidon");
    }
}
