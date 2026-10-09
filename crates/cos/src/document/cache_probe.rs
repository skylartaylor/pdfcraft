//! Opt-in writer experiment, compiled only for unit tests. No product setting or output.

use std::cell::RefCell;

use super::*;

#[derive(Clone, Copy, Debug)]
enum Policy {
    DropAll,
    Lru,
}

#[derive(Default, Debug)]
struct Counts {
    reads: u64,
    loads: u64,
    stream_hits: u64,
    decode_attempts: u64,
    decodes: u64,
    decoded_bytes: u64,
    index_bytes: u64,
    evictions: u64,
    oversized: u64,
    peak_retained_bytes: usize,
    peak_retained_count: usize,
}

struct Probe {
    policy: Policy,
    counts: Counts,
    clock: u64,
    used: HashMap<u32, u64>,
    phase: &'static str,
}

thread_local! {
    static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
}

fn update(f: impl FnOnce(&mut Probe)) {
    PROBE.with(|p| {
        if let Some(p) = p.borrow_mut().as_mut() {
            f(p);
        }
    });
}

pub(super) fn load() {
    update(|p| p.counts.loads += 1);
}

pub(super) fn stream_access(num: u32, hit: bool) {
    update(|p| {
        p.clock += 1;
        p.used.insert(num, p.clock);
        if hit {
            p.counts.stream_hits += 1;
        } else {
            p.counts.decode_attempts += 1;
        }
    });
}

pub(super) fn decoded(stream: &ObjStm) {
    update(|p| {
        p.counts.decodes += 1;
        p.counts.decoded_bytes += stream.data.capacity() as u64;
        p.counts.index_bytes += (stream.index.capacity() * std::mem::size_of::<(u32, usize)>()) as u64;
    });
}

pub(crate) fn phase(phase: &'static str) {
    update(|p| {
        if p.counts.reads != 0 {
            eprintln!("writer_cache {:?} {} {:?}", p.policy, p.phase, p.counts);
        }
        p.phase = phase;
        p.counts = Counts::default();
    });
}

fn bytes(stream: &ObjStm) -> usize {
    stream.data.capacity().saturating_add(stream.index.capacity().saturating_mul(std::mem::size_of::<(u32, usize)>()))
}

pub(super) fn read(reader: &mut ObjectReader, reference: ObjRef) -> Option<Result<Arc<Object>, CosError>> {
    if !PROBE.with(|p| p.borrow().is_some()) {
        return None;
    }
    let object = reader.document.try_get(reference.num);
    let current = match reader.document.xref_entry(reference.num) {
        Some(XrefEntry::InStream { stream, .. }) => Some(stream),
        _ => None,
    };
    let mut streams = reader.document.objstms.lock().unwrap();
    update(|p| {
        p.counts.reads += 1;
        if streams.len() == reader.decoded_count {
            return;
        }
        let before = streams.len();
        let mut retained: usize = streams.values().map(|s| bytes(s)).sum();
        if before != reader.decoded_count {
            match p.policy {
                Policy::DropAll if retained > ObjectReader::STREAM_BYTES || before >= ObjectReader::OBJECTS => {
                    streams.retain(|num, _| Some(*num) == current);
                }
                Policy::Lru => {
                    while retained > ObjectReader::STREAM_BYTES || streams.len() >= ObjectReader::OBJECTS {
                        let oldest = streams.keys().filter(|&&num| Some(num) != current).min_by_key(|num| p.used.get(num)).copied();
                        let Some(oldest) = oldest else { break };
                        if let Some(stream) = streams.remove(&oldest) {
                            retained = retained.saturating_sub(bytes(&stream));
                        }
                    }
                }
                _ => {}
            }
        }
        p.used.retain(|num, _| streams.contains_key(num));
        retained = streams.values().map(|s| bytes(s)).sum();
        p.counts.evictions += (before - streams.len()) as u64;
        p.counts.oversized += u64::from(before != reader.decoded_count && retained > ObjectReader::STREAM_BYTES);
        p.counts.peak_retained_bytes = p.counts.peak_retained_bytes.max(retained);
        p.counts.peak_retained_count = p.counts.peak_retained_count.max(streams.len());
        assert!(streams.len() < ObjectReader::OBJECTS);
        assert!(retained <= ObjectReader::STREAM_BYTES || streams.len() == 1);
    });
    reader.decoded_count = streams.len();
    drop(streams);
    reader.document.cache.lock().unwrap().clear();
    Some(object)
}

fn start(policy: Policy) {
    PROBE.with(|p| *p.borrow_mut() = Some(Probe { policy, counts: Counts::default(), clock: 0, used: HashMap::new(), phase: "read" }));
}

fn stop() -> Counts {
    PROBE.with(|p| p.borrow_mut().take().unwrap().counts)
}

#[test]
#[ignore = "explicit PDFCRAFT_WRITER_PROBE_INPUT and private PDFCRAFT_WRITER_PROBE_DIR required"]
fn compare_full_writer_cache_policies() {
    let input = std::env::var_os("PDFCRAFT_WRITER_PROBE_INPUT").expect("read-only input PDF path");
    let output = std::path::PathBuf::from(std::env::var_os("PDFCRAFT_WRITER_PROBE_DIR").expect("private output directory"));
    std::fs::create_dir_all(&output).unwrap();
    let source = Arc::new(std::fs::read(input).unwrap());
    let doc = Document::open(source).unwrap();
    assert!(doc.repair_log().is_empty(), "use an unrepaired input to compare successful saves");
    for policy in [Policy::DropAll, Policy::Lru] {
        start(policy);
        let saved = crate::write_full(&doc, &crate::SaveOptions::default()).unwrap();
        phase("done");
        stop();
        let name = output.join(format!("{policy:?}.pdf"));
        // Never overwrite an input or previous artifact. The two PDFs should be identical.
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(name).unwrap();
        std::io::Write::write_all(&mut file, &saved).unwrap();
        drop(saved);
    }
}

fn fixture(sizes: &[usize]) -> (Document, Vec<ObjRef>) {
    let mut doc = Document::new_empty();
    let mut references = Vec::new();
    for (i, &size) in sizes.iter().enumerate() {
        let number = 100 + i as u32;
        let container = 1000 + i as u32;
        let header = format!("{number} 0 ");
        let mut data = format!("{header}{i} ").into_bytes();
        data.resize(size.max(data.len()), b' ');
        let mut dict = Dict::new();
        dict.set(b"N".to_vec(), Object::Int(1));
        dict.set(b"First".to_vec(), Object::Int(header.len() as i64));
        doc.set(ObjRef::new(container, 0), Object::Stream(crate::Stream::from_raw(dict, data)));
        Arc::make_mut(&mut doc.entries).insert(number, XrefEntry::InStream { stream: container, index: 0 });
        references.push(ObjRef::new(number, 0));
    }
    (doc, references)
}

#[test]
fn cache_probe_distinguishes_partial_lru_from_drop_all() {
    let (doc, references) = fixture(&[3 * 1024 * 1024; 3]);
    let mut counts = Vec::new();
    for policy in [Policy::DropAll, Policy::Lru] {
        start(policy);
        let mut reader = doc.object_reader();
        for i in [0, 1, 2, 1] {
            assert_eq!(reader.get(references[i]).as_int(), Some(i as i64));
            assert!(reader.document.cache.lock().unwrap().is_empty());
        }
        counts.push(stop());
    }
    assert_eq!(counts[0].decodes, 4);
    assert_eq!(counts[1].decodes, 3);
    assert_eq!(counts[0].evictions, 2);
    assert_eq!(counts[1].evictions, 1);
}

#[test]
fn reader_and_probe_keep_evicted_lookups_and_single_oversized_stream_valid() {
    for policy in [None, Some(Policy::DropAll), Some(Policy::Lru)] {
        let (doc, references) = fixture(&[ObjectReader::STREAM_BYTES + 1, 32]);
        if let Some(policy) = policy {
            start(policy);
        }
        let mut reader = doc.object_reader();
        let held = reader.get(references[0]);
        assert_eq!(held.as_int(), Some(0));
        assert_eq!(reader.document.objstms.lock().unwrap().len(), 1);
        assert!(bytes(reader.document.objstms.lock().unwrap().get(&1000).unwrap()) > ObjectReader::STREAM_BYTES);
        assert_eq!(reader.get(references[1]).as_int(), Some(1));
        assert!(!reader.document.objstms.lock().unwrap().contains_key(&1000));
        assert_eq!(reader.get(references[0]).as_int(), Some(0));
        assert_eq!(held.as_int(), Some(0));
        assert!(reader.document.cache.lock().unwrap().is_empty());
        assert!(doc.objstms.lock().unwrap().is_empty());
        if policy.is_some() {
            assert_eq!(stop().decodes, 3);
        }
        let (doc, references) = fixture(&[32; ObjectReader::OBJECTS + 2]);
        if let Some(policy) = policy {
            start(policy);
        }
        let mut reader = doc.object_reader();
        for (i, reference) in references.iter().enumerate() {
            assert_eq!(reader.get(*reference).as_int(), Some(i as i64));
            assert!(reader.document.objstms.lock().unwrap().len() < ObjectReader::OBJECTS);
        }
        assert_eq!(reader.get(references[0]).as_int(), Some(0));
        if policy.is_some() {
            assert!(stop().evictions > 0);
        }
    }
}

#[test]
fn probe_policies_clear_failed_length_loads_and_obey_decode_limits() {
    for policy in [Policy::DropAll, Policy::Lru] {
        let body = "x".repeat(32 * 1024);
        let stream = format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len());
        let bytes = super::tests::build(
            &["<< /Type /Catalog /Pages 2 0 R >>", "<< /Type /Pages /Kids [] /Count 0 >>", &stream, "<< /Length 3 0 R >>\nstream\nbroken"],
            "/Root 1 0 R",
        );
        let doc = Document::open(Arc::new(bytes)).unwrap();
        start(policy);
        let mut reader = doc.object_reader();
        assert!(reader.try_get(ObjRef::new(4, 0)).is_err());
        assert!(reader.document.cache.lock().unwrap().is_empty());
        stop();

        let (mut doc, references) = fixture(&[32 * 1024]);
        let object = doc.get(ObjRef::new(1000, 0));
        let Object::Stream(stream) = object.as_ref() else { panic!("object stream") };
        doc.set(ObjRef::new(1000, 0), Object::Stream(crate::Stream::flate(stream.dict.clone(), &stream.raw)));
        doc.stream_limit = 256;
        start(policy);
        let mut reader = doc.object_reader();
        assert!(reader.try_get(references[0]).is_err());
        assert!(reader.document.cache.lock().unwrap().is_empty());
        assert!(reader.document.objstms.lock().unwrap().is_empty());
        assert_eq!(stop().decodes, 0);
    }
}
