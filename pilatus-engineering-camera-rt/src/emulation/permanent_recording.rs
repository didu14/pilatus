use std::{future::Future, num::NonZeroU32, time::Duration};

use futures::{
    channel::mpsc::{self, Sender},
    future::Either,
    StreamExt,
};
use pilatus::{
    device::{ActorError, DeviceId, WeakUntypedActorMessageSender},
    Name,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PermanentRecordingConfig {
    pub collection_name: Name,
    pub(super) source_id: DeviceId,
}

impl PermanentRecordingConfig {
    pub(crate) fn collection_path(&self) -> &std::path::Path {
        std::path::Path::new(self.collection_name.as_str())
    }
}

pub(super) fn setup_permanent_recording(
    weak_self_sender: WeakUntypedActorMessageSender,
    params: &Option<PermanentRecordingConfig>,
) -> (
    Sender<Option<PermanentRecordingConfig>>,
    impl Future<Output = ()>,
) {
    let (mut recording_sender, recording_receiver) = mpsc::channel(2);
    recording_sender
        .try_send(params.clone())
        .expect("Just created above with capacity 2");
    (
        recording_sender,
        handle_background_permanent_recording(weak_self_sender, recording_receiver),
    )
}
/// This function doesn't handle recording in 'forward' mode
async fn handle_background_permanent_recording(
    mut self_sender: WeakUntypedActorMessageSender,
    mut recv: mpsc::Receiver<Option<PermanentRecordingConfig>>,
) {
    while let Some(mut maybe_next) = recv.next().await {
        loop {
            let Some(config) = maybe_next.take() else {
                break;
            };
            if config.source_id == self_sender.device_id() {
                warn!("Emulation Camera cannot permanently record itself");
                break;
            }
            let record_task = std::pin::pin!(self_sender.ask(
                pilatus_engineering_camera::RecordMessage::with_max_size(
                    config.source_id.clone(),
                    config.collection_name.clone(),
                    NonZeroU32::MAX,
                ),
            ));
            match futures::future::select(record_task, recv.next()).await {
                Either::Left((Ok(_) | Err(ActorError::UnknownDevice(..)), _)) => {
                    warn!("Recording unknown device");
                    break;
                }
                Either::Left((Err(e), _)) => {
                    warn!("Error during record: {e:?}. Try again in 1s");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    maybe_next = Some(config);
                    continue;
                }
                Either::Right((Some(next_job), _)) => {
                    debug!("Schedule new job: {next_job:?}");
                    maybe_next = next_job;
                    continue;
                }
                Either::Right((None, _)) => {
                    debug!("Permanent Recording shut down (inner loop)");
                    return;
                }
            }
        }
    }
    debug!("Permanent Recording shut down");
}
