use anyhow::Result;
use gpui::{App, AppContext as _, Task};

fn keychain_key(server_id: &str) -> String {
    format!("zenith-deploy:{server_id}")
}

/// Never `zed_credentials_provider::global`: on the Dev channel it stores plaintext JSON.
pub fn save_password(
    server_id: &str,
    username: &str,
    password: &str,
    cx: &App,
) -> Task<Result<()>> {
    cx.write_credentials(&keychain_key(server_id), username, password.as_bytes())
}

pub fn load_password(server_id: &str, cx: &App) -> Task<Result<Option<String>>> {
    let read = cx.read_credentials(&keychain_key(server_id));
    cx.background_spawn(async move {
        Ok(read
            .await?
            .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned()))
    })
}

pub fn delete_password(server_id: &str, cx: &App) -> Task<Result<()>> {
    cx.delete_credentials(&keychain_key(server_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_key_is_namespaced_per_server() {
        assert_eq!(keychain_key("abc"), "zenith-deploy:abc");
    }
}
