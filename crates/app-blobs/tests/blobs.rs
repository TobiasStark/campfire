//! M4-01: Dateien zwischen echten (Loopback-)Knoten.

use app_blobs::{Authorizer, Blobs};
use app_net::NetEvent;
use app_proto::{Hash, PublicKey};
use app_testkit::TestNode;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const T: Duration = Duration::from_secs(20);

/// Stellvertreter fuer "Mitglied des Spaces, zu dem die Datei gehoert".
#[derive(Default)]
struct Members {
    devices: Mutex<HashSet<PublicKey>>,
    hashes: Mutex<HashSet<Hash>>,
    /// Geraete, die gar keine Datei-Verbindung oeffnen duerfen (keine Peer-Verbindung).
    strangers: Mutex<HashSet<PublicKey>>,
}

impl Authorizer for Members {
    fn may_fetch(&self, device: &PublicKey, hash: &Hash) -> bool {
        self.devices.lock().unwrap().contains(device) && self.hashes.lock().unwrap().contains(hash)
    }

    fn may_connect(&self, device: &PublicKey) -> bool {
        !self.strangers.lock().unwrap().contains(device)
    }
}

struct Peer {
    blobs: Arc<Blobs>,
    auth: Arc<Members>,
    dir: PathBuf,
}

fn tmp(name: &str) -> PathBuf {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    let p = std::env::temp_dir().join(format!("campfire-blobs-{name}-{}", hex(&b)));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn data(n: usize) -> Vec<u8> {
    // Nicht komprimierbar und nicht periodisch.
    let mut v = vec![0u8; n];
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for b in v.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x >> 24) as u8;
    }
    v
}

/// Knoten mit Speicher; eingehende Datei-Verbindungen gehen an `Blobs`.
async fn peer(name: &str) -> (TestNode, Peer) {
    let node = TestNode::spawn(name).await;
    let auth = Arc::new(Members::default());
    let dir = tmp(name);
    let blobs = Arc::new(
        Blobs::open(&dir.join("store"), &node.net.endpoint(), auth.clone())
            .await
            .unwrap(),
    );
    (node, Peer { blobs, auth, dir })
}

/// Verbindet `b` mit `a` (b waehlt sich bei a ein) und wartet auf beide Seiten.
async fn link(a: &mut TestNode, b: &mut TestNode) {
    a.allow(b);
    b.allow(a);
    b.net.add_target(a.net.loopback_addr());
    for n in [&mut *a, &mut *b] {
        n.wait_for(T, |e| matches!(e, NetEvent::PeerUp(_)).then_some(()))
            .await
            .expect("PeerUp");
    }
}

/// Leitet Datei-Verbindungen eines Knotens an seinen Speicher.
fn serve(mut node: TestNode, blobs: Arc<Blobs>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(ev) = node.rx.recv().await {
            if let NetEvent::Blobs(conn) = ev {
                blobs.accept(conn);
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn import_gives_blake3_hash_and_stores_in_the_chosen_folder() {
    let node = TestNode::spawn("Eins").await;
    let dir = tmp("import");
    let store = dir.join("blobs");
    let blobs = Blobs::open(&store, &node.net.endpoint(), Arc::new(Members::default()))
        .await
        .unwrap();
    let bytes = data(300_000);
    let f = dir.join("datei.bin");
    std::fs::write(&f, &bytes).unwrap();
    let (hash, size) = blobs.import(&f).await.unwrap();
    assert_eq!(size, 300_000);
    assert_eq!(hash, *iroh_blobs::Hash::new(&bytes).as_bytes());
    assert!(blobs.has(&hash).await);
    assert!(store.exists() && std::fs::read_dir(&store).unwrap().count() > 0);
    // Export liefert dieselben Bytes zurueck.
    let out = dir.join("out.bin");
    blobs.export(&hash, &out).await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn members_download_and_outsiders_are_refused() {
    let (mut a, pa) = peer("Anbieter").await;
    let (mut m, pm) = peer("Mitglied").await;
    let (mut x, px) = peer("Fremd").await;
    link(&mut a, &mut m).await;
    link(&mut a, &mut x).await;

    let bytes = data(2_000_000);
    let f = pa.dir.join("gross.bin");
    std::fs::write(&f, &bytes).unwrap();
    let (hash, _) = pa.blobs.import(&f).await.unwrap();
    // Nur das Geraet des Mitglieds gehoert zum Space der Datei.
    pa.auth.hashes.lock().unwrap().insert(hash);
    pa.auth.devices.lock().unwrap().insert(m.device);
    let a_dev = a.device;
    let _srv = serve(a, pa.blobs.clone());

    let mut last = 0;
    pm.blobs
        .download(&hash, &[a_dev], u64::MAX, |n| last = n)
        .await
        .expect("Mitglied bekommt die Datei");
    let out = pm.dir.join("kopie.bin");
    pm.blobs.export(&hash, &out).await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), bytes);
    assert!(last > 0);

    let r = px.blobs.download(&hash, &[a_dev], u64::MAX, |_| {}).await;
    assert!(r.is_err(), "Fremder darf die Datei nicht bekommen");
    assert!(!px.blobs.has(&hash).await);
    let _ = &mut x;
}

#[tokio::test(flavor = "multi_thread")]
async fn devices_without_peer_connection_cannot_even_connect() {
    let (mut a, pa) = peer("Anbieter2").await;
    let (mut m, pm) = peer("Fremder2").await;
    link(&mut a, &mut m).await;
    let f = pa.dir.join("klein.bin");
    std::fs::write(&f, data(10_000)).unwrap();
    let (hash, _) = pa.blobs.import(&f).await.unwrap();
    // Abholen waere erlaubt, aber das Geraet darf keine Datei-Verbindung oeffnen (M-6).
    pa.auth.hashes.lock().unwrap().insert(hash);
    pa.auth.devices.lock().unwrap().insert(m.device);
    pa.auth.strangers.lock().unwrap().insert(m.device);
    let a_dev = a.device;
    let _srv = serve(a, pa.blobs.clone());
    let r = pm.blobs.download(&hash, &[a_dev], u64::MAX, |_| {}).await;
    assert!(r.is_err(), "Verbindung haette abgelehnt werden muessen");
    pa.auth.strangers.lock().unwrap().clear();
    pm.blobs
        .download(&hash, &[a_dev], u64::MAX, |_| {})
        .await
        .expect("danach erlaubt");
    let _ = &mut m;
}

#[tokio::test(flavor = "multi_thread")]
async fn download_from_several_peers_and_with_one_unreachable() {
    let (mut a, pa) = peer("QuelleA").await;
    let (mut b, pb) = peer("QuelleB").await;
    let (mut c, pc) = peer("Ziel").await;
    link(&mut a, &mut c).await;
    link(&mut b, &mut c).await;
    link(&mut a, &mut b).await;

    let bytes = data(1_500_000);
    let f = pa.dir.join("f.bin");
    std::fs::write(&f, &bytes).unwrap();
    let (hash, _) = pa.blobs.import(&f).await.unwrap();
    // B bekommt die Datei erst von A (also: Weitergabe zwischen Mitgliedern).
    pa.auth.hashes.lock().unwrap().insert(hash);
    for p in [&pa, &pb] {
        p.auth.devices.lock().unwrap().insert(c.device);
        p.auth.hashes.lock().unwrap().insert(hash);
    }
    pa.auth.devices.lock().unwrap().insert(b.device);
    let (a_dev, b_dev) = (a.device, b.device);
    let _sa = serve(a, pa.blobs.clone());
    let _sb = serve(b, pb.blobs.clone());
    let _ = &mut c;

    // B laedt von A.
    pb.blobs
        .download(&hash, &[a_dev], u64::MAX, |_| {})
        .await
        .unwrap();
    // Ziel kennt beide, eine falsche Adresse davor: es klappt trotzdem.
    let ghost = app_testkit::random_key().verifying_key().to_bytes();
    pc.blobs
        .download(&hash, &[ghost, a_dev, b_dev], u64::MAX, |_| {})
        .await
        .expect("Download mit mehreren Quellen");
    let out = pc.dir.join("o.bin");
    pc.blobs.export(&hash, &out).await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_download_continues_instead_of_starting_over() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let (mut a, pa) = peer("Quelle").await;
    let (mut c, pc) = peer("Ziel").await;
    link(&mut a, &mut c).await;
    let size = 60_000_000usize;
    let bytes = data(size);
    let f = pa.dir.join("riesig.bin");
    std::fs::write(&f, &bytes).unwrap();
    let (hash, _) = pa.blobs.import(&f).await.unwrap();
    pa.auth.hashes.lock().unwrap().insert(hash);
    pa.auth.devices.lock().unwrap().insert(c.device);
    let a_dev = a.device;
    let _srv = serve(a, pa.blobs.clone());
    let _ = &mut c;

    // Erster Versuch: nach einigen MB abbrechen.
    let first = Arc::new(AtomicU64::new(0));
    let (blobs, f1) = (pc.blobs.clone(), first.clone());
    let task = tokio::spawn(async move {
        blobs
            .download(&hash, &[a_dev], u64::MAX, move |n| {
                f1.store(n, Ordering::Relaxed)
            })
            .await
    });
    let t0 = std::time::Instant::now();
    while first.load(Ordering::Relaxed) < 4_000_000 && t0.elapsed() < T {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    task.abort();
    let _ = task.await;
    let got = first.load(Ordering::Relaxed);
    assert!(got >= 4_000_000, "Abbruch kam zu spaet: {got}");
    if pc.blobs.has(&hash).await {
        // Auf sehr schnellen Rechnern war der Download schon fertig; dann gibt es nichts fortzusetzen.
        return;
    }
    // Das Angefangene liegt noch im Speicher ...
    let local = pc.blobs.local_bytes(&hash).await;
    assert!(local >= 1_000_000, "nichts behalten: {local}");
    // ... und der zweite Versuch beginnt dort, nicht bei 0.
    let first_seen = Arc::new(AtomicU64::new(u64::MAX));
    let fs2 = first_seen.clone();
    pc.blobs
        .download(&hash, &[a_dev], u64::MAX, move |n| {
            let _ = fs2.compare_exchange(u64::MAX, n, Ordering::Relaxed, Ordering::Relaxed);
        })
        .await
        .unwrap();
    let start = first_seen.load(Ordering::Relaxed);
    println!("beim Abbruch {got} Bytes, lokal {local}, zweiter Versuch beginnt bei {start}");
    assert!(
        start >= local,
        "zweiter Versuch begann bei {start}, lokal waren {local}"
    );
    let out = pc.dir.join("fertig.bin");
    pc.blobs.export(&hash, &out).await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn whoever_has_a_file_passes_it_on_even_when_the_original_source_is_gone() {
    let (mut a, pa) = peer("Urquelle").await;
    let (mut b, pb) = peer("Zwischen").await;
    let (mut c, pc) = peer("Spaeter").await;
    link(&mut a, &mut b).await;
    link(&mut b, &mut c).await;
    let bytes = data(800_000);
    let f = pa.dir.join("w.bin");
    std::fs::write(&f, &bytes).unwrap();
    let (hash, _) = pa.blobs.import(&f).await.unwrap();
    for p in [&pa, &pb] {
        p.auth.hashes.lock().unwrap().insert(hash);
    }
    pa.auth.devices.lock().unwrap().insert(b.device);
    pb.auth.devices.lock().unwrap().insert(c.device);
    let (a_dev, b_dev) = (a.device, b.device);
    let sa = serve(a, pa.blobs.clone());
    let _sb = serve(b, pb.blobs.clone());
    let _ = &mut c;
    pb.blobs
        .download(&hash, &[a_dev], u64::MAX, |_| {})
        .await
        .unwrap();
    // Die Urquelle verschwindet; C bekommt die Datei trotzdem, nur von B.
    sa.abort();
    pc.blobs
        .download(&hash, &[b_dev], u64::MAX, |_| {})
        .await
        .unwrap();
    let out = pc.dir.join("w-kopie.bin");
    pc.blobs.export(&hash, &out).await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn small_bytes_blob_travels_like_a_file_and_can_be_read_back() {
    let (mut a, pa) = peer("Sender").await;
    let (mut m, pm) = peer("Empfaenger").await;
    link(&mut a, &mut m).await;
    let thumb = data(20_000);
    let hash = pa.blobs.import_bytes(thumb.clone()).await.unwrap();
    assert_eq!(hash, *iroh_blobs::Hash::new(&thumb).as_bytes());
    assert_eq!(pa.blobs.read_bytes(&hash).await.unwrap(), thumb);
    pa.auth.hashes.lock().unwrap().insert(hash);
    pa.auth.devices.lock().unwrap().insert(m.device);
    let a_dev = a.device;
    let _srv = serve(a, pa.blobs.clone());
    pm.blobs
        .download(&hash, &[a_dev], u64::MAX, |_| {})
        .await
        .unwrap();
    assert_eq!(pm.blobs.read_bytes(&hash).await.unwrap(), thumb);
}

#[tokio::test(flavor = "multi_thread")]
async fn who_has_lists_only_online_holders_that_serve_us() {
    let (mut a, pa) = peer("Hat").await;
    let (mut c, pc) = peer("HatNicht").await;
    let (mut x, px) = peer("Verweigert").await;
    let (mut m, pm) = peer("Fragt").await;
    link(&mut a, &mut m).await;
    link(&mut c, &mut m).await;
    link(&mut x, &mut m).await;
    let f = pa.dir.join("w.bin");
    std::fs::write(&f, data(100_000)).unwrap();
    let (h, _) = pa.blobs.import(&f).await.unwrap();
    // X hat die Datei auch, liefert sie aber nicht an uns aus.
    let (hx, _) = px.blobs.import(&f).await.unwrap();
    assert_eq!(h, hx);
    pa.auth.hashes.lock().unwrap().insert(h);
    pa.auth.devices.lock().unwrap().insert(m.device);
    pc.auth.hashes.lock().unwrap().insert(h);
    pc.auth.devices.lock().unwrap().insert(m.device);
    let devs = [a.device, c.device, x.device];
    let _s1 = serve(a, pa.blobs.clone());
    let _s2 = serve(c, pc.blobs.clone());
    let _s3 = serve(x, px.blobs.clone());
    let got = pm.blobs.who_has(&h, &devs, Duration::from_secs(5)).await;
    assert_eq!(
        got.len(),
        1,
        "nur A hat die Datei und gibt sie her: {got:?}"
    );
    assert_eq!(got[0], devs[0]);
    let _ = &mut m;
}

#[tokio::test(flavor = "multi_thread")]
async fn evicted_files_are_deleted_by_cleanup_but_everything_else_stays() {
    let (mut a, pa) = peer("Quelle").await;
    let node = TestNode::spawn("Cache").await;
    let dir = tmp("cache");
    let cache = Arc::new(
        Blobs::open_with_gc(
            &dir.join("store"),
            &node.net.endpoint(),
            Arc::new(Members::default()),
            Duration::from_secs(1),
        )
        .await
        .unwrap(),
    );
    let mut b = node;
    link(&mut a, &mut b).await;
    // Zwei grosse Dateien (liegen als eigene Dateien im Speicher) kommen per Download.
    let mut hashes = Vec::new();
    for i in 0..2u8 {
        let f = pa.dir.join(format!("g{i}.bin"));
        let mut bytes = data(400_000);
        bytes[0] = i;
        std::fs::write(&f, &bytes).unwrap();
        let (h, _) = pa.blobs.import(&f).await.unwrap();
        pa.auth.hashes.lock().unwrap().insert(h);
        hashes.push(h);
    }
    pa.auth.devices.lock().unwrap().insert(b.device);
    let a_dev = a.device;
    let _srv = serve(a, pa.blobs.clone());
    for h in &hashes {
        cache.download(h, &[a_dev], u64::MAX, |_| {}).await.unwrap();
    }
    // Kleine Daten, die selbst importiert wurden (mit Tag).
    let own = cache.import_bytes(data(1000)).await.unwrap();

    cache.evict(&hashes[0]).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while cache.has(&hashes[0]).await && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(!cache.has(&hashes[0]).await, "entfernte Datei ist weg");
    // Zwei weitere Aufraeumlaeufe abwarten: der Rest darf nicht verschwinden.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(cache.has(&hashes[1]).await, "heruntergeladene Datei bleibt");
    assert!(cache.has(&own).await);
    // Wieder herunterladen geht.
    cache
        .download(&hashes[0], &[a_dev], u64::MAX, |_| {})
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(cache.has(&hashes[0]).await, "neu geladene Datei bleibt");
}

#[tokio::test(flavor = "multi_thread")]
async fn download_stops_when_the_file_is_larger_than_allowed() {
    // Die Groesse im FileShare (bzw. "Vorschaubild <= 100 KB") behauptet nur der Absender. Zeigt
    // der Hash auf eine grosse Datei, bricht der Download an der Grenze ab.
    let (mut a, pa) = peer("Gross").await;
    let (mut m, pm) = peer("Klein").await;
    link(&mut a, &mut m).await;
    let bytes = data(8_000_000);
    let f = pa.dir.join("riesig.bin");
    std::fs::write(&f, &bytes).unwrap();
    let (hash, _) = pa.blobs.import(&f).await.unwrap();
    pa.auth.hashes.lock().unwrap().insert(hash);
    pa.auth.devices.lock().unwrap().insert(m.device);
    let a_dev = a.device;
    let _srv = serve(a, pa.blobs.clone());

    let mut last = 0;
    let r = pm
        .blobs
        .download(&hash, &[a_dev], 256 * 1024, |n| last = n)
        .await;
    assert!(matches!(r, Err(app_blobs::BlobError::TooLarge)), "{r:?}");
    assert!(!pm.blobs.has(&hash).await);
    assert!(last <= 256 * 1024, "{last}");
    // Mit passender Grenze klappt es (angefangene Teile werden weiterverwendet).
    pm.blobs
        .download(&hash, &[a_dev], bytes.len() as u64, |_| {})
        .await
        .unwrap();
    assert!(pm.blobs.has(&hash).await);
}
