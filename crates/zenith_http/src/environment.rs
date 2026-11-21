use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentStore {
    pub environments: HashMap<String, HashMap<String, String>>,
    pub active_environment: Option<String>,
}

impl EnvironmentStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from_json(
        &mut self,
        public_json: &str,
        private_json: Option<&str>,
    ) -> Result<(), String> {
        let mut parsed: HashMap<String, HashMap<String, String>> = HashMap::new();

        if !public_json.trim().is_empty() {
            let val: Value = serde_json::from_str(public_json)
                .map_err(|e| format!("Invalid public env JSON: {e}"))?;
            if let Value::Object(map) = val {
                for (env_name, env_val) in map {
                    if let Value::Object(var_map) = env_val {
                        let mut vars = HashMap::new();
                        for (k, v) in var_map {
                            if let Value::String(s) = v {
                                vars.insert(k, s);
                            } else {
                                vars.insert(k, v.to_string());
                            }
                        }
                        parsed.insert(env_name, vars);
                    }
                }
            }
        }

        if let Some(priv_str) = private_json {
            if !priv_str.trim().is_empty() {
                let val: Value = serde_json::from_str(priv_str)
                    .map_err(|e| format!("Invalid private env JSON: {e}"))?;
                if let Value::Object(map) = val {
                    for (env_name, env_val) in map {
                        if let Value::Object(var_map) = env_val {
                            let vars = parsed.entry(env_name).or_default();
                            for (k, v) in var_map {
                                if let Value::String(s) = v {
                                    vars.insert(k, s);
                                } else {
                                    vars.insert(k, v.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }

        self.environments = parsed;
        // No environment is picked for the user: an implicit choice could be `prod`. A
        // selection that no longer exists is dropped.
        if self
            .active_environment
            .as_ref()
            .is_some_and(|name| !self.environments.contains_key(name))
        {
            self.active_environment = None;
        }

        Ok(())
    }

    /// Environment names, sorted, for a picker.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.environments.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn get_active_variables(&self) -> HashMap<String, String> {
        if let Some(ref env_name) = self.active_environment {
            self.environments.get(env_name).cloned().unwrap_or_default()
        } else {
            HashMap::new()
        }
    }

    pub fn set_active_environment(&mut self, name: Option<String>) {
        self.active_environment = name;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_environment_merging() {
        let public_json = r#"{
            "dev": {
                "host": "http://localhost:8000",
                "api_key": "public-dev-key"
            },
            "prod": {
                "host": "https://api.prod.com",
                "api_key": "public-prod-key"
            }
        }"#;

        let private_json = r#"{
            "dev": {
                "api_key": "secret-override-key"
            }
        }"#;

        let mut store = EnvironmentStore::new();
        store
            .load_from_json(public_json, Some(private_json))
            .expect("load env");

        store.set_active_environment(Some("dev".to_string()));
        let dev_vars = store.get_active_variables();
        assert_eq!(dev_vars.get("host").unwrap(), "http://localhost:8000");
        assert_eq!(dev_vars.get("api_key").unwrap(), "secret-override-key");

        store.set_active_environment(Some("prod".to_string()));
        let prod_vars = store.get_active_variables();
        assert_eq!(prod_vars.get("api_key").unwrap(), "public-prod-key");
    }

    #[test]
    fn test_no_environment_is_chosen_implicitly() {
        let mut store = EnvironmentStore::new();
        store
            .load_from_json(
                r#"{"dev": {"h": "d"}, "production": {"h": "p"}, "a-prod": {"h": "x"}}"#,
                None,
            )
            .unwrap();
        assert_eq!(store.active_environment, None);
        assert!(store.get_active_variables().is_empty());
        assert_eq!(store.names(), vec!["a-prod", "dev", "production"]);

        store.set_active_environment(Some("dev".to_string()));
        store
            .load_from_json(r#"{"dev": {"h": "d2"}}"#, None)
            .unwrap();
        assert_eq!(
            store.get_active_variables()["h"],
            "d2",
            "a kept selection survives a reload"
        );

        store
            .load_from_json(r#"{"staging": {"h": "s"}}"#, None)
            .unwrap();
        assert_eq!(
            store.active_environment, None,
            "a removed environment is deselected, not replaced"
        );
    }
}
