// Serialize / deserialize speed + size comparison across five encoders on one
// realistic mixed payload:
//
//                binary                         text
//   micro_serde  serialize_bin/deserialize_bin  serialize_json, serialize_ron
//   asun         encode_binary/decode_binary    encode / decode
//
// micro_serde uses its own hand-rolled Ser*/De* derives (no serde dep, with a
// memcpy fast path for POD Vecs). asun is a serde 1.x format crate. So every
// benchmark struct derives BOTH trait families; the same value is fed to all
// five encoders and each round-trip is asserted equal before timing.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
// Glob import: the Ser*/De* derive macros expand to code that refers to helper
// types (DeBinErr, SerJsonState, DeRonState, …) by bare name, so they must all
// be in scope, not just the traits.
use makepad_micro_serde::*;
// asun's native derives. A single AsunEncode/AsunDecode pair emits BOTH the text
// (encode/decode) and binary (encode_binary/decode_binary) trait impls.
use asun::{AsunDecode, AsunEncode};

// A representative record: ints, a float, a String, a small enum-ish tag, and a
// nested struct — the shape real config / IPC payloads take.
#[derive(Clone, PartialEq, Debug, SerBin, DeBin, SerJson, DeJson, SerRon, DeRon, AsunEncode, AsunDecode)]
struct Point {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Clone, PartialEq, Debug, SerBin, DeBin, SerJson, DeJson, SerRon, DeRon, AsunEncode, AsunDecode)]
struct Record {
    id: u64,
    name: String,
    active: bool,
    score: f64,
    tags: Vec<u32>,
    position: Point,
}

#[derive(Clone, PartialEq, Debug, SerBin, DeBin, SerJson, DeJson, SerRon, DeRon, AsunEncode, AsunDecode)]
struct Payload {
    version: u32,
    records: Vec<Record>,
}

// Build N records deterministically (no rng — the seeded arithmetic keeps the
// data reproducible across runs and cheap to construct).
fn make_payload(n: usize) -> Payload {
    let records = (0..n)
        .map(|i| Record {
            id: i as u64 * 2654435761,
            name: format!("record_{i}_αβγ"), // include multibyte to exercise escaping
            active: i % 2 == 0,
            score: (i as f64) * 1.5 - 0.25,
            tags: (0..(i % 8)).map(|t| (t * 7 + i) as u32).collect(),
            position: Point {
                x: i as f32 * 0.5,
                y: i as f32 * -0.25,
                z: i as f32 + 0.125,
            },
        })
        .collect();
    Payload { version: 1, records }
}

// Assert every encoder round-trips to an equal value, and print encoded sizes
// once. A broken path (or a format that silently loses precision) would fail
// here rather than produce a misleading timing.
fn verify_and_report_sizes(p: &Payload) {
    let bin = p.serialize_bin();
    let json = p.serialize_json();
    let ron = p.serialize_ron();
    let asun_txt = asun::encode(p).expect("asun encode");
    let asun_bin = asun::encode_binary(p).expect("asun encode_binary");

    assert_eq!(&Payload::deserialize_bin(&bin).unwrap(), p, "micro_serde bin");
    assert_eq!(&Payload::deserialize_json(&json).unwrap(), p, "micro_serde json");
    assert_eq!(&Payload::deserialize_ron(&ron).unwrap(), p, "micro_serde ron");
    assert_eq!(&asun::decode::<Payload>(&asun_txt).unwrap(), p, "asun text");
    assert_eq!(&asun::decode_binary::<Payload>(&asun_bin).unwrap(), p, "asun binary");

    eprintln!("\n=== encoded size for {} records (bytes) ===", p.records.len());
    eprintln!("  micro_serde bin  : {:>9}", bin.len());
    eprintln!("  asun binary      : {:>9}", asun_bin.len());
    eprintln!("  micro_serde json : {:>9}", json.len());
    eprintln!("  asun text        : {:>9}", asun_txt.len());
    eprintln!("  micro_serde ron  : {:>9}", ron.len());
    eprintln!();
}

fn bench(c: &mut Criterion) {
    let payload = make_payload(1000);
    verify_and_report_sizes(&payload);

    // ---- serialize ----
    let mut ser = c.benchmark_group("serialize");
    ser.throughput(Throughput::Elements(payload.records.len() as u64));
    ser.bench_function("micro_serde/bin", |b| b.iter(|| black_box(black_box(&payload).serialize_bin())));
    ser.bench_function("asun/binary", |b| b.iter(|| black_box(asun::encode_binary(black_box(&payload)).unwrap())));
    ser.bench_function("micro_serde/json", |b| b.iter(|| black_box(black_box(&payload).serialize_json())));
    ser.bench_function("asun/text", |b| b.iter(|| black_box(asun::encode(black_box(&payload)).unwrap())));
    ser.bench_function("micro_serde/ron", |b| b.iter(|| black_box(black_box(&payload).serialize_ron())));
    ser.finish();

    // ---- deserialize (encode once outside the loop) ----
    let bin = payload.serialize_bin();
    let json = payload.serialize_json();
    let ron = payload.serialize_ron();
    let asun_txt = asun::encode(&payload).unwrap();
    let asun_bin = asun::encode_binary(&payload).unwrap();

    let mut de = c.benchmark_group("deserialize");
    de.throughput(Throughput::Elements(payload.records.len() as u64));
    de.bench_function("micro_serde/bin", |b| {
        b.iter(|| black_box(Payload::deserialize_bin(black_box(&bin)).unwrap()))
    });
    de.bench_function("asun/binary", |b| {
        b.iter(|| black_box(asun::decode_binary::<Payload>(black_box(&asun_bin)).unwrap()))
    });
    de.bench_function("micro_serde/json", |b| {
        b.iter(|| black_box(Payload::deserialize_json(black_box(&json)).unwrap()))
    });
    de.bench_function("asun/text", |b| {
        b.iter(|| black_box(asun::decode::<Payload>(black_box(&asun_txt)).unwrap()))
    });
    de.bench_function("micro_serde/ron", |b| {
        b.iter(|| black_box(Payload::deserialize_ron(black_box(&ron)).unwrap()))
    });
    de.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
