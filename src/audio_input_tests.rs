use super::*;
use crate::{
    api::{Api, Origin},
    audio::{Audio, MasterGain},
    player::{Authority, Status},
};
use hyper::body::Bytes;
use rodio::mixer::{MixerSource, mixer};
use tokio::{
    sync::{mpsc, oneshot},
    time::Instant,
};

const RATE: u32 = 8000;
const DATA: u32 = 34 * 1024 * 1024;
const LEN: u64 = DATA as u64 + 44;
const SAMPLE: [u8; 2] = 12000i16.to_le_bytes();

fn wav() -> [u8; 44] {
    let mut header = [0u8; 44];
    header[..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(DATA + 36).to_le_bytes());
    header[8..16].copy_from_slice(b"WAVEfmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes());
    header[22..24].copy_from_slice(&1u16.to_le_bytes());
    header[24..28].copy_from_slice(&RATE.to_le_bytes());
    header[28..32].copy_from_slice(&(RATE * 2).to_le_bytes());
    header[32..34].copy_from_slice(&2u16.to_le_bytes());
    header[34..36].copy_from_slice(&16u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&DATA.to_le_bytes());
    header
}

fn anonymous_fixture() -> io::Result<File> {
    let path = std::env::temp_dir().join(format!("spofify-source-test-{}", uuid::Uuid::new_v4()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::fs::remove_file(path)?;
    Ok(file)
}

fn body_fixture() -> (crate::api::Session, mpsc::Sender<Bytes>, AudioBody) {
    // Run the real HTTP-body consumer with a controlled streaming body, no
    // actual network, user store, or audio device. api.rs covers the wire.
    let api = Api::new(Origin::parse("https://fixture.invalid").unwrap()).unwrap();
    let session = api.test_session();
    let (send, receive) = mpsc::channel(1);
    let chunks = futures_util::stream::unfold(receive, |mut rx| async move {
        rx.recv().await.map(|chunk| (Ok(chunk), rx))
    });
    let body = AudioBody::test_stream(&session, LEN, chunks);
    (session, send, body)
}

fn pcm_block<const N: usize>() -> [u8; N] {
    std::array::from_fn(|i| SAMPLE[i % 2])
}

async fn first_pcm(mixer: &mut MixerSource) -> f32 {
    timeout(Duration::from_secs(10), async {
        loop {
            for _ in 0..256 {
                if let Some(sample) = mixer.next()
                    && sample > 0.3
                {
                    return sample;
                }
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("playable PCM")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn online_large_wav_starts_before_tail_and_seeks_and_ends_on_one_body() {
    let (session, writer, body) = body_fixture();
    let (send_tail, gate) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let data = Bytes::from(pcm_block::<{ 64 * 1024 }>().to_vec());
        writer.send(Bytes::copy_from_slice(&wav())).await.unwrap();
        writer.send(data.clone()).await.unwrap();
        gate.await.unwrap();
        for _ in 1..DATA as usize / data.len() {
            writer.send(data.clone()).await.unwrap();
        }
    });
    let started_at = Instant::now();
    let source = AudioInput::spool(body, anonymous_fixture).await.unwrap();
    assert_eq!(source.len(), LEN);
    let (controller, mut samples) = mixer(1, RATE);
    let mut audio = Audio::new(
        Some(session),
        false,
        Arc::new(AtomicU64::new(1)),
        1,
        MasterGain::default(),
    );
    audio.use_test_streaming_output(controller, 1, RATE);
    assert_eq!(
        audio.play("big".into(), source.clone(), 0.0, 0.0, Authority::offline()),
        Status::Ok
    );
    assert!(first_pcm(&mut samples).await > 0.3);
    let startup = started_at.elapsed();
    assert!(audio.has_started() && audio.facts().elapsed > 0.0);
    assert!(!source.cacheable(), "the tail has not been sent");
    assert!(source.is_stream() && !source.is_failed());
    assert!(
        source.isolated().is_none(),
        "a written prefix is not enough"
    );

    let duration = DATA as f64 / (2 * RATE) as f64;
    let seek = duration - 0.025;
    assert_eq!(
        audio.play(
            "big".into(),
            source.clone(),
            seek,
            duration,
            Authority::offline()
        ),
        Status::Ok
    );
    for _ in 0..1024 {
        assert!(samples.next().is_none_or(|sample| sample == 0.0));
    }
    let buffering = audio.facts();
    assert!(buffering.loading && !buffering.ended && !audio.has_started());
    assert_eq!(buffering.elapsed, seek);
    let sent_at = Instant::now();
    send_tail.send(()).unwrap();
    assert!(first_pcm(&mut samples).await > 0.3);
    let seek_wait = sent_at.elapsed();
    timeout(Duration::from_secs(5), async {
        while !audio.facts().ended {
            for _ in 0..256 {
                let _ = samples.next();
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(audio.failure(), None);
    assert_eq!(audio.facts().elapsed, duration);
    assert!(source.cacheable());
    let isolated = source
        .isolated()
        .expect("complete spool is reusable offline");
    assert!(isolated._transfer.is_none());
    drop((audio, source));
    let mut reader = isolated.reader();
    reader.seek(SeekFrom::End(-2)).unwrap();
    let mut tail = [0; 2];
    reader.read_exact(&mut tail).unwrap();
    assert_eq!(tail, SAMPLE);
    server.await.unwrap();
    eprintln!(
        "34 MiB online: prefix PCM at {startup:?}, far seek after releasing tail in {seek_wait:?}"
    );
}

#[tokio::test]
async fn online_mode_change_or_last_owner_drop_closes_a_stalled_response() {
    for mode_change in [false, true] {
        let (session, writer, body) = body_fixture();
        writer.send(Bytes::copy_from_slice(&wav())).await.unwrap();
        let source = AudioInput::spool(body, anonymous_fixture).await.unwrap();
        if mode_change {
            session.set_offline(true);
            timeout(Duration::from_secs(2), async {
                while !source.is_failed() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        // Model a retired decoder whose descriptor outlives the actor's owner.
        let decoder = source.decoder_input();
        let held_reader = decoder.reader();
        drop(source);
        timeout(Duration::from_secs(2), writer.closed())
            .await
            .unwrap();
        drop((held_reader, decoder));
    }
}

#[tokio::test]
async fn online_short_body_is_not_success_or_cacheable() {
    let (_, writer, body) = body_fixture();
    writer.send(Bytes::copy_from_slice(&wav())).await.unwrap();
    drop(writer);
    let source = AudioInput::spool(body, anonymous_fixture).await.unwrap();
    let worker_source = source.clone();
    let terminal = tokio::task::spawn_blocking(move || worker_source.await_complete(|| true));
    assert!(
        timeout(Duration::from_secs(2), terminal)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(!source.cacheable());
    assert!(source.isolated().is_none());
}

#[test]
fn unverified_file_is_not_an_isolated_input() {
    let file = anonymous_fixture().unwrap();
    file.write_all_at(&SAMPLE, 0).unwrap();
    let source = AudioInput::file(file, 0, SAMPLE.len() as u64);
    assert!(source.isolated().is_none());
}
