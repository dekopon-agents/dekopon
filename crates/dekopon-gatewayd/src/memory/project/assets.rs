use super::*;
use crate::{
    asset::{MemoryAssets, SummaryInput},
    memory::{journal::tools::asset_available, record::ArrivedAsset},
};
use dekopon_core::{AssetNumber, asset::AssetKind};
use dekopon_model::{asset::BlobReference, model::ContentPart};

pub(super) struct Row {
    pub number: AssetNumber,
    pub item: u32,
    pub slots: Vec<Slot>,
    pub pin: Option<Rung>,
    pub rung: Rung,
    pub shown: Rung,
    kind: AssetKind,
    parts: usize,
    native: Option<ModelMessage>,
    native_bytes: u64,
    unresident: bool,
    unshowable: bool,
}

#[derive(Default)]
pub(crate) struct Counts {
    pub rungs: RungCounts,
    pub native_bytes: u64,
    pub omitted: u64,
    pub native_unresident: u64,
    pub native_unshowable: u64,
    pub lines: u64,
}

impl Row {
    fn ladder(&self) -> Vec<Rung> {
        let native = match self.kind {
            AssetKind::Image | AssetKind::BinaryDocument => true,
            AssetKind::TextDocument | AssetKind::Other => false,
        };
        let mut rungs = asset_available(&self.slots, false);
        rungs.pop();
        rungs.reverse();
        rungs.push(Rung::Metadata);
        if native {
            rungs.insert(0, Rung::Native);
        }
        rungs
    }

    fn start(&self) -> Rung {
        let ladder = self.ladder();
        self.pin
            .filter(|pin| ladder.contains(pin))
            .unwrap_or_else(|| {
                ladder
                    .iter()
                    .find_map(|rung| match rung {
                        Rung::Summary(_) => Some(*rung),
                        Rung::Native | Rung::Metadata => None,
                    })
                    .unwrap_or(Rung::Metadata)
            })
    }

    pub(super) fn new(
        asset: &ArrivedAsset,
        head: &Head,
        prior: Option<&PriorRecord>,
        stable: bool,
    ) -> Self {
        let state = asset.join(&head.asset_lines);
        let mut row = Self {
            number: asset.number,
            item: asset.item,
            slots: state.summaries,
            pin: state.pin,
            rung: Rung::Metadata,
            shown: Rung::Metadata,
            kind: asset.kind,
            parts: head
                .header
                .plan
                .as_ref()
                .map_or(asset.summaries.len(), |plan| plan.parts.len()),
            native: None,
            native_bytes: 0,
            unresident: false,
            unshowable: false,
        };
        row.rung = prior
            .filter(|_| stable)
            .and_then(|prior| {
                prior
                    .assets
                    .iter()
                    .find(|prior| prior.number == asset.number && prior.pin == row.pin)
            })
            .map(|prior| prior.rung)
            .filter(|rung| row.ladder().contains(rung))
            .unwrap_or_else(|| row.start());
        row.shown = row.rung;
        row
    }

    pub(super) fn reset(&mut self) {
        self.rung = self.start();
        self.shown = self.rung;
    }

    pub(super) fn prepare(&mut self, head: &Head, access: Option<MemoryAssets<'_>>, visible: bool) {
        self.shown = self.rung;
        self.unresident = false;
        self.unshowable = false;
        if !visible || self.rung != Rung::Native {
            return;
        }
        let showable = match self.kind {
            AssetKind::Image => access.is_some_and(|access| access.images_supported),
            AssetKind::BinaryDocument => true,
            AssetKind::TextDocument | AssetKind::Other => false,
        };
        if !showable {
            self.unshowable = true;
        } else if let Some(access) = access
            && access.store.is_resident(access.access, self.number)
            && let Ok(input) = access
                .store
                .summary_input(access.access, self.number, self.kind)
        {
            let (mime, blob, image) = match input {
                SummaryInput::Image { mime, blob } => (mime, blob, true),
                SummaryInput::Binary { mime, blob } => (mime, blob, false),
                SummaryInput::Text(_) => return,
            };
            self.native_bytes = blob.len() as u64;
            let data = BlobReference::new(
                Arc::new(blob),
                self.native_bytes as usize,
                self.number.get(),
            );
            let part = if image {
                ContentPart::Image { mime, data }
            } else {
                let name = head
                    .header
                    .assets
                    .iter()
                    .find(|asset| asset.id == self.number.get())
                    .map_or_else(
                        || format!("chat-asset-{}", self.number.get()),
                        |asset| asset.name.clone(),
                    );
                ContentPart::File { name, mime, data }
            };
            self.native = Some(ModelMessage::user_with_parts(vec![
                ContentPart::Text(format!("chat-asset:{}:", self.number.get())),
                part,
            ]));
            return;
        } else {
            self.unresident = true;
        }
        self.shown = self.ladder().get(1).copied().unwrap_or(Rung::Metadata);
        self.native = None;
    }

    pub(super) fn line(&self) -> Option<String> {
        match self.shown {
            Rung::Summary(part) => match self.slots.get(usize::from(part.get() - 1)) {
                Some(Slot::Generated(text)) => Some(format!(
                    "[gateway: chat-asset:{} summary {}/{} — {text}]",
                    self.number.get(),
                    part.get(),
                    self.parts
                )),
                Some(Slot::Queued | Slot::Running | Slot::Error(_)) | None => None,
            },
            Rung::Native | Rung::Metadata => None,
        }
    }

    pub(super) fn text_bytes(&self) -> u64 {
        self.line().map_or(0, |line| line.len() as u64)
    }
    pub(super) fn bytes(&self) -> u64 {
        if self.shown == Rung::Native {
            self.native_bytes
        } else {
            0
        }
    }
    pub(super) fn step(&mut self) -> bool {
        let ladder = self.ladder();
        let next = ladder
            .iter()
            .position(|rung| *rung == self.shown)
            .and_then(|at| ladder.get(at + 1))
            .copied();
        if let Some(next) = next {
            self.rung = next;
            self.shown = next;
            self.native = None;
            true
        } else {
            false
        }
    }
    pub(super) fn message(&mut self) -> Option<ModelMessage> {
        match self.shown {
            Rung::Native => self.native.take(),
            Rung::Summary(_) => self.line().map(ModelMessage::user),
            Rung::Metadata => None,
        }
    }
    pub(super) fn count(&self, counts: &mut Counts, visible: bool) {
        if !visible {
            counts.omitted += 1;
            return;
        }
        match self.shown {
            Rung::Native => {
                counts.rungs.native += 1;
                counts.native_bytes += self.native_bytes;
            }
            Rung::Summary(_) => counts.rungs.summary += 1,
            Rung::Metadata => counts.rungs.metadata += 1,
        }
        counts.native_unresident += u64::from(self.unresident);
        counts.native_unshowable += u64::from(self.unshowable);
    }
}
