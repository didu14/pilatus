use std::collections::HashMap;
use std::{fs::File, io::Write, sync::Arc};

use futures::{sink::SinkExt, StreamExt};
use minfac::{Registered, ServiceCollection};
use pilatus::{
    device::{ActorSystem, DeviceContext, DeviceResult, DeviceValidationContext},
    prelude::*,
    DeviceConfig, RecipeId, UpdateParamsMessageError,
};
use pilatus_rt::{RecipeServiceFassade, Runtime};
use reqwest::StatusCode;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[test]
fn upload_zip() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    let mut file = File::create(dir.path().join("config.json"))?;
    file.write_all(
        br#"{
            "web": {
                "socket": "0.0.0.0:0"
            },
            "tracing": {
                "default_level": "debug",
                "filters": {
                  "tokio": "info",
                  "mio_serial": "info",
                  "mio::poll": "debug",
                  "runtime::resource": "debug",
                  "pilatus_anyfeeder": "error",
                  "pilatus::image": "debug",
                  "tower_http": "trace"
                }
            }
        }"#,
    )?;
    file.flush().unwrap();

    extern "C" fn register_test_services(c: &mut ServiceCollection) {
        async fn device(
            ctx: DeviceContext,
            params: i32,
            actor_system: ActorSystem,
        ) -> DeviceResult {
            actor_system.register(ctx.id).execute(params).await;
            Ok(())
        }
        async fn validator(
            x: DeviceValidationContext<'_>,
        ) -> Result<i32, UpdateParamsMessageError> {
            x.params_as_sealed::<i32>()
        }
        c.with::<Registered<ActorSystem>>()
            .register_device("testdevice", validator, device);
    }

    let rt = Runtime::with_root(dir.path())
        .register(pilatus_axum_rt::register)
        .register(register_test_services)
        .configure();

    let web_stats: pilatus_axum::Stats = rt.provider.get().unwrap();
    let recipe_service = rt.provider.get().unwrap();
    rt.run_until_finished(async {
        let port = web_stats.socket_addr().await.port();
        assert_ne!(port, 80);
        let base = format!("http://127.0.0.1:{port}/api");
        let client = reqwest::Client::new();
        let (clone_id, data) = generate_zip(&base, &client, recipe_service).await.unwrap();

        let (mut sock, _response) =
            connect_async(format!("ws://127.0.0.1:{port}/api/recipe/import"))
                .await
                .unwrap();

        sock.send(tokio_tungstenite::tungstenite::Message::Binary(
            (data.len() as u64).to_le_bytes().to_vec(),
        ))
        .await
        .unwrap();
        for chunk in data.chunks(3) {
            sock.send(tokio_tungstenite::tungstenite::Message::Binary(
                chunk.to_vec(),
            ))
            .await
            .unwrap()
        }
        let answer = sock.next().await;
        let Some(Ok(Message::Text(msg))) = answer else {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            panic!("Expected text response {answer:?}");
        };
        assert_eq!(msg, "\"Success\"");

        // Websocket-Workers exist even if Axum shut down causes ServiceProvider to crash
        let (_, all) = get_current(&base, &client).await.unwrap();
        assert!(all.contains(&clone_id));
        //tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    });
    Ok(())
}

async fn generate_zip(
    base: &str,
    client: &reqwest::Client,
    s: Arc<RecipeServiceFassade>,
) -> anyhow::Result<(RecipeId, Vec<u8>)> {
    let (active_id, _) = get_current(base, client).await?;
    let clone_response_body = client
        .put(format!("{base}/recipe/{}/clone", active_id))
        .send()
        .await?
        .bytes()
        .await?;

    let clone_id: RecipeId = serde_json::from_value(
        serde_json::from_slice::<serde_json::Value>(&clone_response_body[..])?
            .get(0)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Unknown index '0' in {}",
                    String::from_utf8_lossy(&clone_response_body[..])
                )
            })?
            .clone(),
    )?;

    // Execute with http is not yet implemented
    s.add_device_to_recipe(clone_id.clone(), DeviceConfig::mock(42))
        .await?;

    let export_response_body = client
        .get(format!("{base}/recipe/{}/export", clone_id))
        .send()
        .await?
        .bytes()
        .await?
        .to_vec();

    assert!(!export_response_body.is_empty());

    let delete_response_status = client
        .delete(format!("{base}/recipe/{}", clone_id))
        .send()
        .await?
        .status();
    assert_eq!(delete_response_status, StatusCode::OK);

    Ok((clone_id, export_response_body))
}

async fn get_current(
    base: &str,
    client: &reqwest::Client,
) -> Result<(RecipeId, Vec<RecipeId>), anyhow::Error> {
    let get_active_response_body = client
        .get(format!("{base}/recipe/get_all"))
        .send()
        .await?
        .bytes()
        .await?;
    let json_response = serde_json::from_slice::<serde_json::Value>(&get_active_response_body[..])?;
    let active_id: RecipeId = serde_json::from_value(
        json_response
            .get("active_id")
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Unknown field 'active_id' in {}",
                    String::from_utf8_lossy(&get_active_response_body[..])
                )
            })?
            .clone(),
    )?;

    let map = json_response.get("all").ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown field 'all' in {}",
            String::from_utf8_lossy(&get_active_response_body[..])
        )
    })?;

    let all_recipe_json: HashMap<RecipeId, serde_json::Value> =
        serde_json::from_value(map.clone())?;

    Ok((active_id, all_recipe_json.into_keys().collect()))
}
