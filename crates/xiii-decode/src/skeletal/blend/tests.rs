use super::*;
use crate::skeletal::normalize::{Bone, BoneTrack};

fn skeleton() -> Skeleton {
    let bones = [
        ("root", None),
        ("arm", Some(0)),
        ("hand", Some(1)),
        ("leg", Some(0)),
    ]
    .into_iter()
    .map(|(name, parent)| Bone {
        name: name.into(),
        parent,
        flags: 0,
        bind_local: Transform::IDENTITY,
        bind_global: Transform::IDENTITY,
    })
    .collect();
    Skeleton { bones }
}
fn clip(name: &str, x: f32) -> Clip {
    Clip {
        name: name.into(),
        groups: vec![],
        rate: 1.0,
        num_frames: 4,
        track_time: 4.0,
        duration: 4.0,
        root_speed: Vec3::ZERO,
        notifies: vec![],
        tracks: skeleton()
            .bones
            .iter()
            .map(|b| BoneTrack {
                bone: b.name.clone(),
                times: vec![0.0, 3.0],
                rotations: vec![Quat::IDENTITY; 2],
                positions: vec![Vec3::new(x, 0.0, 0.0); 2],
            })
            .collect(),
    }
}
fn channel(stage: u8, name: &str) -> Channel {
    Channel {
        stage,
        sample: Sample {
            sequence: name.into(),
            frame: 0.0,
            looping: false,
            tween_alpha: 1.0,
            source: None,
        },
        alpha: 1.0,
        in_time: 0.0,
        bone: None,
    }
}
fn close(a: Vec3, b: Vec3) {
    assert!((a - b).length() < 1e-4, "{a:?} != {b:?}");
}

#[test]
fn zero_rotation_alpha_allows_location_and_negative_alpha_has_no_effect() {
    let s = skeleton();
    let controls = [Controller {
        bone: "arm".into(),
        rotation: Some(([0, 16384, 0], 0.0)),
        translation: Some((Vec3::new(2.0, 0.0, 0.0), 0.5)),
        scale: None,
    }];
    let pose = evaluate(&s, &[], &controls, |_| None).unwrap();
    close(pose.locals[1].translation, Vec3::new(1.0, 0.0, 0.0));
    close(
        pose.locals[1].rotation.rotate(Vec3::new(1.0, 0.0, 0.0)),
        Vec3::new(1.0, 0.0, 0.0),
    );
    let mut controls = controls;
    controls[0].translation.as_mut().unwrap().1 = -1.0;
    close(
        evaluate(&s, &[], &controls, |_| None).unwrap().locals[1].translation,
        Vec3::ZERO,
    );
    controls[0].rotation.as_mut().unwrap().1 = f32::NAN;
    assert!(evaluate(&s, &[], &controls, |_| None).is_err());
}

#[test]
fn tween_and_interrupted_tween_keep_frozen_source() {
    let (a, b) = (clip("A", 2.0), clip("B", 10.0));
    let mut c = channel(0, "B");
    c.sample.tween_alpha = 0.25;
    c.sample.source = Some(Box::new(channel(0, "A").sample));
    let lookup = |s: &str| if s == "A" { Some(&a) } else { Some(&b) };
    let p = evaluate(&skeleton(), &[c.clone()], &[], lookup).unwrap();
    close(p.locals[0].translation, Vec3::new(4.0, 0.0, 0.0));
    let frozen = c.sample.clone();
    c.sample.sequence = "A".into();
    c.sample.source = Some(Box::new(frozen));
    c.sample.tween_alpha = 0.5;
    close(
        evaluate(&skeleton(), &[c], &[], lookup).unwrap().locals[0].translation,
        Vec3::new(3.0, 0.0, 0.0),
    );
}

#[test]
fn channels_sort_and_filter_subtree_without_overwriting_missing_tracks() {
    let (a, mut b) = (clip("A", 2.0), clip("B", 10.0));
    b.tracks.retain(|t| t.bone != "hand");
    let mut upper = channel(2, "B");
    upper.bone = Some("ARM".into());
    upper.alpha = 0.5;
    let p = evaluate(&skeleton(), &[upper, channel(0, "A")], &[], |s| {
        if s == "A" { Some(&a) } else { Some(&b) }
    })
    .unwrap();
    for (i, x) in [2.0, 6.0, 2.0, 2.0].into_iter().enumerate() {
        close(p.locals[i].translation, Vec3::new(x, 0.0, 0.0));
    }
}

#[test]
fn loop_wrap_interpolates_and_nonloop_holds_last_key() {
    let mut c = clip("A", 0.0);
    c.tracks[0].positions[1].x = 12.0;
    close(
        c.tracks[0].sample(3.5, 4.0, true).translation,
        Vec3::new(6.0, 0.0, 0.0),
    );
    close(c.tracks[0].sample(4.0, 4.0, true).translation, Vec3::ZERO);
    close(
        c.tracks[0].sample(3.5, 4.0, false).translation,
        Vec3::new(12.0, 0.0, 0.0),
    );
}

#[test]
fn rotation_controller_and_scaled_attachment_use_parent_coordinates() {
    let s = skeleton();
    let c = Controller {
        bone: "arm".into(),
        rotation: Some(([0, 16384, 0], 1.0)),
        scale: Some(Vec3::new(2.0, 1.0, 1.0)),
        ..Default::default()
    };
    let p = evaluate(&s, &[], &[c], |_| None).unwrap();
    let mesh = Coords::new(
        Transform::new(rotator_quat([0, 16384, 0]), Vec3::new(100.0, 0.0, 0.0)),
        Vec3::new(1.0, 1.0, 1.0),
    );
    let t = attachment(
        mesh,
        p.globals[2],
        Transform::new(rotator_quat([0, 16384, 0]), Vec3::new(3.0, 0.0, 0.0)),
    );
    close(t.origin, Vec3::new(94.0, 0.0, 0.0));
    close(t.axes[0], Vec3::new(0.0, -1.0, 0.0));
}

#[test]
fn invalid_hierarchy_tracks_and_requests_are_errors() {
    let mut s = skeleton();
    s.bones[1].parent = Some(1);
    assert!(evaluate(&s, &[], &[], |_| None).is_err());
    let s = skeleton();
    assert!(evaluate(&s, &[channel(0, "Missing")], &[], |_| None).is_err());
    let mut c = clip("A", 0.0);
    c.tracks[0].times.clear();
    assert!(evaluate(&s, &[channel(0, "A")], &[], |_| Some(&c)).is_err());
    assert!(
        evaluate(
            &s,
            &[],
            &[Controller {
                bone: "missing".into(),
                ..Default::default()
            }],
            |_| None
        )
        .is_err()
    );
    let mut bad = channel(0, "A");
    bad.sample.frame = f32::NAN;
    assert!(evaluate(&s, &[bad], &[], |_| Some(&c)).is_err());
}

#[test]
fn blend_in_is_sequence_fraction_and_zero_scale_is_valid() {
    let a = clip("A", 8.0);
    let mut c = channel(1, "A");
    c.sample.frame = 1.0;
    c.in_time = 0.5;
    let p = evaluate(
        &skeleton(),
        &[c],
        &[Controller {
            bone: "arm".into(),
            scale: Some(Vec3::ZERO),
            ..Default::default()
        }],
        |_| Some(&a),
    )
    .unwrap();
    close(p.locals[0].translation, Vec3::new(4.0, 0.0, 0.0));
    close(p.globals[1].axes[0], Vec3::ZERO);
    assert!(
        evaluate(&Skeleton::default(), &[], &[], |_| None)
            .unwrap()
            .globals
            .is_empty()
    );
}
