use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::File,
    io::{Cursor, Read},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use gpui::RenderImage;
use image::{
    DynamicImage, Frame, ImageDecoder, ImageFormat, ImageReader, Limits, imageops::FilterType,
};
use reqwest::{Url, blocking::Client, redirect::Policy};
use tokio::sync::Notify;

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SOURCE_SIDE: u32 = 4096;
const MAX_THUMB_SIDE: u32 = 512;
const FAILURE_RETRY_DELAY: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum CoverSource {
    Remote(String),
    Local(PathBuf),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CoverTicket {
    source: CoverSource,
    width: u32,
    height: u32,
    revision: u64,
}

#[derive(Clone, Debug)]
pub struct CoverReady {
    pub ticket: CoverTicket,
    pub available: bool,
}

struct Work {
    ticket: CoverTicket,
}
struct Complete {
    ticket: CoverTicket,
    image: Option<Arc<RenderImage>>,
    bytes: usize,
}
struct Cached {
    image: Option<Arc<RenderImage>>,
    bytes: usize,
    used: u64,
    failed_at: Option<Instant>,
}

pub struct CoverCache {
    max_bytes: usize,
    max_entries: usize,
    workers: usize,
    send: mpsc::SyncSender<Work>,
    recv: mpsc::Receiver<Complete>,
    completed: Arc<Notify>,
    wanted: VecDeque<CoverTicket>,
    active: HashSet<CoverTicket>,
    visible: HashSet<CoverTicket>,
    running: HashSet<CoverTicket>,
    entries: HashMap<CoverTicket, Cached>,
    disposed: Vec<Arc<RenderImage>>,
    total_bytes: usize,
    clock: u64,
    revision: u64,
    #[cfg(test)]
    ownership_updates: usize,
}

impl CoverCache {
    pub fn new(max_bytes: usize, workers: usize) -> Self {
        let workers = workers.clamp(1, 4);
        let (send, work_recv) = mpsc::sync_channel::<Work>(workers * 2);
        let (complete_send, recv) = mpsc::sync_channel::<Complete>(workers * 2);
        let completed = Arc::new(Notify::new());
        let work_recv = Arc::new(Mutex::new(work_recv));
        for _ in 0..workers {
            let work_recv = work_recv.clone();
            let complete_send = complete_send.clone();
            let completed = completed.clone();
            thread::spawn(move || {
                let client = remote_client();
                loop {
                    let work = { work_recv.lock().unwrap().recv() };
                    let Ok(work) = work else { break };
                    let (image, bytes) = load_thumbnail(&work.ticket, client.as_ref());
                    if complete_send
                        .send(Complete {
                            ticket: work.ticket,
                            image,
                            bytes,
                        })
                        .is_err()
                    {
                        break;
                    }
                    completed.notify_one();
                }
            });
        }
        Self {
            max_bytes: max_bytes.max(1),
            max_entries: (max_bytes / 8192).clamp(128, 4096),
            workers,
            send,
            recv,
            completed,
            wanted: VecDeque::new(),
            active: HashSet::new(),
            visible: HashSet::new(),
            running: HashSet::new(),
            entries: HashMap::new(),
            disposed: Vec::new(),
            total_bytes: 0,
            clock: 0,
            revision: 0,
            #[cfg(test)]
            ownership_updates: 0,
        }
    }

    // Dispatch follows ownership reconciliation or polling so batches prioritize visible work.
    pub fn request(
        &mut self,
        source: CoverSource,
        width_px: u32,
        height_px: u32,
        scale: f32,
        visible: bool,
    ) -> CoverTicket {
        let scale = if scale.is_finite() && scale > 0. {
            scale
        } else {
            1.
        };
        let ticket = CoverTicket {
            source,
            width: ((width_px as f32 * scale).ceil() as u32).clamp(1, MAX_THUMB_SIDE),
            height: ((height_px as f32 * scale).ceil() as u32).clamp(1, MAX_THUMB_SIDE),
            revision: self.revision,
        };
        self.active.insert(ticket.clone());
        if visible {
            self.visible.insert(ticket.clone());
        }
        if !self.entries.contains_key(&ticket)
            && !self.running.contains(&ticket)
            && !self.wanted.contains(&ticket)
            && (visible || self.wanted.len() < 128)
        {
            self.wanted.push_back(ticket.clone());
        }
        ticket
    }

    pub fn completion_signal(&self) -> Arc<Notify> {
        self.completed.clone()
    }

    pub fn retain_visible(&mut self, tickets: &[CoverTicket]) {
        self.retain_active(tickets, &[]);
    }

    pub fn retain_active(&mut self, visible: &[CoverTicket], nearby: &[CoverTicket]) {
        #[cfg(test)]
        {
            self.ownership_updates += 1;
        }
        self.visible = visible
            .iter()
            .filter(|ticket| ticket.revision == self.revision)
            .cloned()
            .collect();
        self.active = self.visible.clone();
        self.active.extend(
            nearby
                .iter()
                .filter(|ticket| ticket.revision == self.revision)
                .cloned(),
        );
        self.wanted.retain(|ticket| self.active.contains(ticket));
        self.evict();
        self.pump();
    }

    #[cfg(test)]
    pub fn audit_ownership_updates(&self) -> usize {
        self.ownership_updates
    }

    pub fn poll(&mut self) -> Vec<CoverReady> {
        let mut ready = Vec::new();
        while let Ok(complete) = self.recv.try_recv() {
            self.running.remove(&complete.ticket);
            if complete.ticket.revision != self.revision {
                if let Some(image) = complete.image {
                    self.disposed.push(image);
                }
                continue;
            }
            self.clock = self.clock.wrapping_add(1);
            self.total_bytes += complete.bytes;
            let available = complete.image.is_some();
            self.entries.insert(
                complete.ticket.clone(),
                Cached {
                    image: complete.image,
                    bytes: complete.bytes,
                    used: self.clock,
                    failed_at: (!available).then(Instant::now),
                },
            );
            if self.active.contains(&complete.ticket) {
                ready.push(CoverReady {
                    ticket: complete.ticket,
                    available,
                });
            }
        }
        self.retry_failed_active();
        self.evict();
        self.pump();
        ready
    }

    pub fn image(&mut self, ticket: &CoverTicket) -> Option<Arc<RenderImage>> {
        let entry = self.entries.get_mut(ticket)?;
        self.clock = self.clock.wrapping_add(1);
        entry.used = self.clock;
        entry.image.clone()
    }

    pub fn take_disposed(&mut self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut self.disposed)
    }

    pub fn clear(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.active.clear();
        self.visible.clear();
        self.wanted.clear();
        self.total_bytes = 0;
        for (_, entry) in self.entries.drain() {
            if let Some(image) = entry.image {
                self.disposed.push(image);
            }
        }
    }

    fn pump(&mut self) {
        while self.running.len() < self.workers {
            let next = self
                .wanted
                .iter()
                .position(|ticket| self.visible.contains(ticket))
                .unwrap_or(0);
            let Some(ticket) = self.wanted.remove(next) else {
                break;
            };
            if self.entries.contains_key(&ticket) || self.running.contains(&ticket) {
                continue;
            }
            match self.send.try_send(Work {
                ticket: ticket.clone(),
            }) {
                Ok(()) => {
                    self.running.insert(ticket);
                }
                Err(mpsc::TrySendError::Full(_)) => {
                    self.wanted.push_front(ticket);
                    break;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
    }

    fn retry_failed_active(&mut self) {
        let now = Instant::now();
        let due: Vec<_> = self
            .entries
            .iter()
            .filter(|(ticket, entry)| {
                self.active.contains(*ticket)
                    && !self.running.contains(*ticket)
                    && !self.wanted.contains(*ticket)
                    && entry
                        .failed_at
                        .is_some_and(|at| now.duration_since(at) >= FAILURE_RETRY_DELAY)
            })
            .map(|(ticket, _)| ticket.clone())
            .collect();
        for ticket in due {
            self.entries.remove(&ticket);
            if self.visible.contains(&ticket) || self.wanted.len() < 128 {
                self.wanted.push_back(ticket);
            }
        }
    }

    fn evict(&mut self) {
        while self.total_bytes > self.max_bytes
            || self.entries.len() > self.max_entries.max(self.visible.len())
        {
            let Some(ticket) = self
                .entries
                .iter()
                .filter(|(ticket, _)| !self.visible.contains(*ticket))
                .min_by_key(|(_, entry)| entry.used)
                .map(|(ticket, _)| ticket.clone())
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&ticket) {
                self.total_bytes = self.total_bytes.saturating_sub(entry.bytes);
                if let Some(image) = entry.image {
                    self.disposed.push(image);
                }
            }
        }
    }
}

fn remote_client() -> Option<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(8))
        .redirect(Policy::custom(|attempt| {
            if !permitted_redirect(attempt.url(), attempt.previous().len()) {
                attempt.error("Cover redirect is not permitted")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .ok()
}

fn load_thumbnail(
    ticket: &CoverTicket,
    client: Option<&Client>,
) -> (Option<Arc<RenderImage>>, usize) {
    let bytes = match &ticket.source {
        CoverSource::Local(path) => {
            let Ok(file) = File::open(path) else {
                return (None, 0);
            };
            let mut bytes = Vec::new();
            if file
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
                .is_err()
            {
                return (None, 0);
            }
            bytes
        }
        CoverSource::Remote(url) => {
            let Ok(url) = Url::parse(url) else {
                return (None, 0);
            };
            if !permitted_url(&url) {
                return (None, 0);
            }
            let Some(client) = client else {
                return (None, 0);
            };
            let Ok(response) = client
                .get(url)
                .send()
                .and_then(|response| response.error_for_status())
            else {
                return (None, 0);
            };
            if response
                .content_length()
                .is_some_and(|length| length > MAX_FILE_BYTES)
            {
                return (None, 0);
            }
            let mut bytes = Vec::new();
            if response
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
                .is_err()
            {
                return (None, 0);
            }
            bytes
        }
    };
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return (None, 0);
    }
    let Some(image) = thumbnail(&bytes, ticket.width, ticket.height) else {
        return (None, 0);
    };
    let approximate_bytes = image.1;
    (Some(image.0), approximate_bytes)
}

fn permitted_url(url: &Url) -> bool {
    #[cfg(feature = "fixture-runtime")]
    if url.scheme() == "http"
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url.host_str().is_some_and(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        })
    {
        return true;
    }
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none_or(|port| port == 443)
        && url
            .host_str()
            .is_some_and(|host| host == "anilist.co" || host.ends_with(".anilist.co"))
}

fn permitted_redirect(url: &Url, previous_count: usize) -> bool {
    previous_count <= 3 && permitted_url(url)
}

fn thumbnail(bytes: &[u8], width: u32, height: u32) -> Option<(Arc<RenderImage>, usize)> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    if !matches!(reader.format(), Some(ImageFormat::Jpeg | ImageFormat::Png)) {
        return None;
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().ok()?;
    let mut decoded = DynamicImage::from_decoder(decoder).ok()?;
    decoded.apply_orientation(orientation);
    let ratio = (width as f64 / decoded.width() as f64)
        .max(height as f64 / decoded.height() as f64)
        .min(1.)
        .min(MAX_THUMB_SIDE as f64 / decoded.width().max(decoded.height()) as f64);
    let size = (
        (decoded.width() as f64 * ratio).round().max(1.) as u32,
        (decoded.height() as f64 * ratio).round().max(1.) as u32,
    );
    let mut pixels = if size == (decoded.width(), decoded.height()) {
        decoded.into_rgba8()
    } else {
        decoded
            .resize_exact(size.0, size.1, FilterType::Lanczos3)
            .into_rgba8()
    };
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let bytes = pixels.len();
    Some((Arc::new(RenderImage::new(vec![Frame::new(pixels)])), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    fn controlled_cache(
        max_bytes: usize,
        workers: usize,
    ) -> (CoverCache, mpsc::Receiver<Work>, mpsc::SyncSender<Complete>) {
        let (send, work) = mpsc::sync_channel(workers * 2);
        let (complete, recv) = mpsc::sync_channel(workers * 2);
        let mut cache = CoverCache::new(max_bytes, workers);
        cache.send = send;
        cache.recv = recv;
        (cache, work, complete)
    }

    fn request(cache: &mut CoverCache, source: &str, visible: bool) -> CoverTicket {
        cache.request(CoverSource::Remote(source.into()), 1, 1, 1., visible)
    }

    fn complete(ticket: &CoverTicket) -> Complete {
        Complete {
            ticket: ticket.clone(),
            image: Some(Arc::new(RenderImage::new(vec![Frame::new(
                ImageBuffer::from_pixel(1, 1, Rgba([1, 2, 3, 255])),
            )]))),
            bytes: 4,
        }
    }

    #[test]
    fn visible_promotion_precedes_prefetch_and_keeps_fifo_order() {
        let (mut cache, work, completed) = controlled_cache(16, 1);
        let running = request(&mut cache, "running", true);
        cache.retain_visible(std::slice::from_ref(&running));
        assert_eq!(work.try_recv().unwrap().ticket, running);
        let nearby = request(&mut cache, "nearby", false);
        let promoted = request(&mut cache, "promoted", false);
        let visible = request(&mut cache, "visible", true);
        assert_eq!(request(&mut cache, "promoted", true), promoted);
        cache.retain_active(
            &[running.clone(), promoted.clone(), visible.clone()],
            std::slice::from_ref(&nearby),
        );
        completed.send(complete(&running)).unwrap();
        cache.poll();
        assert_eq!(work.try_recv().unwrap().ticket, promoted);
        completed.send(complete(&promoted)).unwrap();
        cache.poll();
        assert_eq!(work.try_recv().unwrap().ticket, visible);
        completed.send(complete(&visible)).unwrap();
        cache.poll();
        assert_eq!(work.try_recv().unwrap().ticket, nearby);
        assert!(work.try_recv().is_err());
    }

    #[test]
    fn ownership_batch_discards_departed_work_and_demotes_prefetch() {
        let (mut cache, work, _) = controlled_cache(16, 2);
        let departed = request(&mut cache, "departed", true);
        let demoted = request(&mut cache, "demoted", true);
        let visible = request(&mut cache, "visible", false);
        assert!(work.try_recv().is_err());
        cache.retain_active(
            std::slice::from_ref(&visible),
            std::slice::from_ref(&demoted),
        );
        assert_eq!(work.try_recv().unwrap().ticket, visible);
        assert_eq!(work.try_recv().unwrap().ticket, demoted);
        assert!(!cache.running.contains(&departed));
        assert!(cache.wanted.is_empty());
    }

    #[test]
    fn departed_completion_is_reused_without_pinning_or_redownloading() {
        let (mut cache, work, completed) = controlled_cache(8, 1);
        let ticket = request(&mut cache, "returning", true);
        cache.retain_visible(std::slice::from_ref(&ticket));
        assert_eq!(work.try_recv().unwrap().ticket, ticket);
        cache.retain_visible(&[]);
        let completion = complete(&ticket);
        let image = completion.image.clone().unwrap();
        completed.send(completion).unwrap();
        assert!(cache.poll().is_empty());
        assert!(cache.active.is_empty());
        assert_eq!(request(&mut cache, "returning", true), ticket);
        cache.retain_visible(std::slice::from_ref(&ticket));
        assert!(Arc::ptr_eq(&cache.image(&ticket).unwrap(), &image));
        assert!(work.try_recv().is_err());
        let next = request(&mut cache, "next", true);
        cache.retain_visible(std::slice::from_ref(&next));
        assert_eq!(work.try_recv().unwrap().ticket, next);
        completed.send(complete(&next)).unwrap();
        cache.poll();
        cache.retain_visible(&[]);
        let last = request(&mut cache, "last", true);
        cache.retain_visible(std::slice::from_ref(&last));
        assert_eq!(work.try_recv().unwrap().ticket, last);
        completed.send(complete(&last)).unwrap();
        cache.poll();
        assert_eq!(cache.total_bytes, 8);
        assert!(cache.image(&ticket).is_none());
        assert!(cache.image(&next).is_some());
        assert!(cache.image(&last).is_some());
        assert_eq!(cache.take_disposed().len(), 1);
    }

    #[test]
    fn clear_rejects_inflight_images_and_releases_the_worker_for_new_revision() {
        let (mut cache, work, completed) = controlled_cache(8, 1);
        let old = request(&mut cache, "same-source", true);
        cache.retain_visible(std::slice::from_ref(&old));
        assert_eq!(work.try_recv().unwrap().ticket, old);
        cache.clear();
        let current = request(&mut cache, "same-source", true);
        assert_ne!(old, current);
        cache.retain_visible(std::slice::from_ref(&current));
        assert!(work.try_recv().is_err());
        completed.send(complete(&old)).unwrap();
        assert!(cache.poll().is_empty());
        assert_eq!(cache.total_bytes, 0);
        assert_eq!(cache.take_disposed().len(), 1);
        assert_eq!(work.try_recv().unwrap().ticket, current);
        completed.send(complete(&current)).unwrap();
        let ready = cache.poll();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].ticket, current);
        assert!(ready[0].available);
        assert!(cache.image(&old).is_none());
    }

    #[test]
    fn invalidation_handles_all_workers_finishing_out_of_order() {
        let (mut cache, work, completed) = controlled_cache(32, 4);
        let old: Vec<_> = (0..4)
            .map(|i| request(&mut cache, &format!("cover-{i}"), true))
            .collect();
        cache.retain_visible(&old);
        for ticket in &old {
            assert_eq!(&work.try_recv().unwrap().ticket, ticket);
        }
        cache.clear();
        let current: Vec<_> = (0..4)
            .map(|i| request(&mut cache, &format!("cover-{i}"), true))
            .collect();
        cache.retain_visible(&current);
        assert!(work.try_recv().is_err());
        for (next, index) in [2, 0, 3, 1].into_iter().enumerate() {
            completed.send(complete(&old[index])).unwrap();
            assert!(cache.poll().is_empty());
            assert_eq!(work.try_recv().unwrap().ticket, current[next]);
            assert_eq!(cache.total_bytes, 0);
        }
        assert_eq!(cache.take_disposed().len(), 4);
        for index in [3, 1, 0, 2] {
            completed.send(complete(&current[index])).unwrap();
        }
        assert_eq!(cache.poll().len(), 4);
        assert_eq!(cache.total_bytes, 16);
        for (old, current) in old.iter().zip(&current) {
            assert!(cache.image(old).is_none());
            assert!(cache.image(current).is_some());
        }
        assert!(cache.running.is_empty());
    }

    #[test]
    fn worker_completion_wakes_a_waiter_without_a_poll_timer() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut cache = CoverCache::new(8, 1);
        let completed = cache.completion_signal();
        let first = request(&mut cache, "invalid-first", true);
        let next = request(&mut cache, "invalid-next", true);
        cache.retain_visible(&[first.clone(), next.clone()]);
        for ticket in [first, next] {
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), completed.notified())
                    .await
                    .unwrap();
            });
            let ready = cache.poll();
            assert_eq!(ready.len(), 1);
            assert_eq!(ready[0].ticket, ticket);
            assert!(!ready[0].available);
        }
        assert!(cache.running.is_empty());
    }

    #[test]
    fn local_cover_fixtures_require_an_explicit_build_feature() {
        let local = Url::parse("http://127.0.0.1:41001/cover.png").unwrap();
        assert_eq!(permitted_url(&local), cfg!(feature = "fixture-runtime"));
        for address in [
            "http://localhost:41001/cover.png",
            "http://192.168.1.2/cover.png",
            "http://127.0.0.1.example.test/cover.png",
            "http://name@127.0.0.1/cover.png",
            "http://127.0.0.1/cover.png#fragment",
        ] {
            assert!(!permitted_url(&Url::parse(address).unwrap()), "{address}");
        }
    }

    #[test]
    fn all_visible_covers_survive_a_small_budget() {
        let mut cache = CoverCache::new(1, 2);
        let tickets: Vec<_> = (0..80)
            .map(|i| {
                cache.request(
                    CoverSource::Local(format!("missing-{i}").into()),
                    46,
                    63,
                    1.,
                    true,
                )
            })
            .collect();
        cache.retain_visible(&tickets);
        assert_eq!(cache.visible.len(), 80);
        assert!(cache.wanted.len() + cache.running.len() >= 78);
    }

    #[test]
    fn rejects_non_anilist_and_non_https_remote_sources() {
        assert!(permitted_url(
            &Url::parse("https://s4.anilist.co/file/cover.jpg").unwrap()
        ));
        assert!(!permitted_url(
            &Url::parse("http://s4.anilist.co/file/cover.jpg").unwrap()
        ));
        assert!(!permitted_url(
            &Url::parse("https://s4.anilist.co.evil.invalid/cover.jpg").unwrap()
        ));
        assert!(!permitted_url(
            &Url::parse("https://user:pass@s4.anilist.co/cover.jpg").unwrap()
        ));
        assert!(!permitted_url(
            &Url::parse("https://s4.anilist.co:8443/cover.jpg").unwrap()
        ));
        assert!(permitted_redirect(
            &Url::parse("https://media.anilist.co/cover.jpg").unwrap(),
            3
        ));
        assert!(!permitted_redirect(
            &Url::parse("https://media.anilist.co/cover.jpg").unwrap(),
            4
        ));
        assert!(!permitted_redirect(
            &Url::parse("https://other.invalid/cover.jpg").unwrap(),
            1
        ));
    }

    #[test]
    fn thumbnail_decodes_png_at_display_scale_and_rejects_corruption() {
        let image =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(200, 300, Rgba([12, 34, 56, 255])));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        let (decoded, byte_count) = thumbnail(bytes.get_ref(), 92, 126).unwrap();
        let size = decoded.size(0);
        assert_eq!((size.width.0, size.height.0), (92, 138));
        assert_eq!(byte_count, 92 * 138 * 4);
        assert!(thumbnail(b"broken image", 46, 63).is_none());
    }

    #[test]
    fn visible_entries_can_exceed_budget_without_eviction_loop() {
        let mut cache = CoverCache::new(4, 1);
        let tickets: Vec<_> = (0..64)
            .map(|index| {
                cache.request(
                    CoverSource::Local(format!("missing-{index}").into()),
                    46,
                    63,
                    1.,
                    true,
                )
            })
            .collect();
        cache.retain_visible(&tickets);
        for ticket in &tickets {
            cache.entries.insert(
                ticket.clone(),
                Cached {
                    image: None,
                    bytes: 4,
                    used: 0,
                    failed_at: None,
                },
            );
            cache.total_bytes += 4;
        }
        cache.evict();
        assert_eq!(cache.entries.len(), 64);
        cache.retain_visible(&tickets[..2]);
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.total_bytes, 8);
    }

    #[test]
    fn nearby_covers_do_not_pin_decoded_images_over_budget() {
        let mut cache = CoverCache::new(8, 1);
        let tickets: Vec<_> = (0..4)
            .map(|index| {
                cache.request(
                    CoverSource::Local(format!("missing-prefetch-{index}").into()),
                    1,
                    1,
                    1.,
                    false,
                )
            })
            .collect();
        let images: Vec<_> = (0..4)
            .map(|_| {
                Arc::new(RenderImage::new(vec![Frame::new(ImageBuffer::from_pixel(
                    1,
                    1,
                    Rgba([1, 2, 3, 255]),
                ))]))
            })
            .collect();
        for (index, ticket) in tickets.iter().enumerate() {
            cache.entries.insert(
                ticket.clone(),
                Cached {
                    image: Some(images[index].clone()),
                    bytes: 4,
                    used: index as u64,
                    failed_at: None,
                },
            );
            cache.total_bytes += 4;
        }
        cache.retain_active(&tickets[..1], &tickets[1..]);
        assert!(cache.entries.contains_key(&tickets[0]));
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.total_bytes, 8);
        drop(cache.take_disposed());
        assert_eq!(Arc::strong_count(&images[1]), 1);
        assert_eq!(Arc::strong_count(&images[2]), 1);
    }

    #[test]
    fn failed_entries_have_a_bounded_count() {
        let mut cache = CoverCache::new(1, 1);
        for index in 0..300 {
            let ticket = cache.request(
                CoverSource::Local(format!("missing-{index}").into()),
                46,
                63,
                1.,
                false,
            );
            cache.entries.insert(
                ticket,
                Cached {
                    image: None,
                    bytes: 0,
                    used: index,
                    failed_at: None,
                },
            );
        }
        cache.evict();
        assert_eq!(cache.entries.len(), 128);
    }

    #[test]
    fn active_failed_cover_retries_after_delay_without_repinning_old_rows() {
        let mut cache = CoverCache::new(8, 1);
        let active = CoverTicket {
            source: CoverSource::Local("missing-active".into()),
            width: 1,
            height: 1,
            revision: cache.revision,
        };
        let inactive = CoverTicket {
            source: CoverSource::Local("missing-inactive".into()),
            width: 1,
            height: 1,
            revision: cache.revision,
        };
        for ticket in [&active, &inactive] {
            cache.entries.insert(
                ticket.clone(),
                Cached {
                    image: None,
                    bytes: 0,
                    used: 0,
                    failed_at: Some(Instant::now() - FAILURE_RETRY_DELAY),
                },
            );
        }
        cache.retain_active(std::slice::from_ref(&active), &[]);
        cache.poll();
        assert!(!cache.entries.contains_key(&active));
        assert!(cache.running.contains(&active) || cache.wanted.contains(&active));
        assert!(cache.entries.contains_key(&inactive));
        assert!(!cache.active.contains(&inactive));
    }
}
