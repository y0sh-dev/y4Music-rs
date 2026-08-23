use songbird::input::core::io::{MediaSource, ReadOnlySource};
use songbird::input::{
    AudioStream, ChildContainer, LiveInput, RawAdapter,
    codecs::{get_codec_registry, get_probe},
};
use std::process::{Command, Stdio};

fn promote_from_children(children: Vec<std::process::Child>) -> Result<(), String> {
    let container = ChildContainer::from(children);
    let source = ReadOnlySource::new(container);
    let raw = RawAdapter::new(source, 48_000, 2);
    let stream = AudioStream {
        input: Box::new(raw) as Box<dyn MediaSource>,
    };
    LiveInput::Raw(stream)
        .promote(get_codec_registry(), get_probe())
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

#[test]
fn promotes_from_a_single_ffmpeg_stage() {
    let stage = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-vn",
            "-af",
            "equalizer=f=80:width_type=o:width=2:g=5,equalizer=f=12000:width_type=o:width=2:g=4",
            "-f",
            "f32le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ffmpeg");

    assert!(promote_from_children(vec![stage]).is_ok());
}

#[test]
fn promotes_from_a_valid_two_stage_pipe() {
    let mut stage1 = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:a",
            "libopus",
            "-f",
            "webm",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn stage1");
    let stage1_stdout = stage1.stdout.take().unwrap();

    let stage2 = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "pipe:0",
            "-vn",
            "-af",
            "equalizer=f=80:width_type=o:width=2:g=5,equalizer=f=12000:width_type=o:width=2:g=4",
            "-f",
            "f32le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "pipe:1",
        ])
        .stdin(Stdio::from(stage1_stdout))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn stage2");

    assert!(promote_from_children(vec![stage1, stage2]).is_ok());
}

#[test]
fn promotes_even_when_the_ffmpeg_stage_fails_outright() {
    let mut stage1 = Command::new("sh")
        .args(["-c", "head -c 4096 /dev/urandom"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn stage1");
    let stage1_stdout = stage1.stdout.take().unwrap();

    let stage2 = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "pipe:0",
            "-vn",
            "-f",
            "f32le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "pipe:1",
        ])
        .stdin(Stdio::from(stage1_stdout))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn stage2");

    assert!(promote_from_children(vec![stage1, stage2]).is_ok());
}
