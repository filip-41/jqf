//! Cooperative codec execution context and session terminal vocabulary.
//!
//! [`CodecRunContext`] borrows the request's resources for one decode or encode call. Sibling: [`crate::access`].

use jqf_resource::ResourceContext;

const ITEM_TRAILER_CAP: usize = 8;

/// The straight-line codec execution context: one borrow over an independently-lived request context, carrying the
/// cooperative credit budget a straight-line decode/encode replenishes at its own loop heads.
///
/// The SDK sets the budget once per drive from its `cooperative_credits` policy value; a codec whose work meter reports
/// `Pending` calls [`Self::replenish_work`] instead of yielding to a host that never polls between quanta. The
/// admission check runs at the parser loop heads and every replenish observes control, so cancellation and the deadline
/// are seen at the same granularity a yielding drive would see them.
pub struct CodecRunContext<'ctx, 'control> {
    resources: &'ctx mut ResourceContext<'control>,
    cooperative_credits: u32,
    /// Facade item suffix copied into the encoder's last staging write so payload and terminator publish as one hop.
    /// Empty when the suffix is longer than this buffer; the drive then publishes it after encode.
    item_trailer: [u8; ITEM_TRAILER_CAP],
    item_trailer_len: u8,
}

impl<'ctx, 'control> CodecRunContext<'ctx, 'control> {
    /// Binds one straight-line codec call to request resources and control. The cooperative budget starts unset (zero):
    /// a codec that exhausts its work meter without a configured budget reports the internal-contract violation rather
    /// than looping forever. Every production drive sets it before calling decode/encode.
    #[must_use]
    pub const fn new(resources: &'ctx mut ResourceContext<'control>) -> Self {
        Self {
            resources,
            cooperative_credits: 0,
            item_trailer: [0; ITEM_TRAILER_CAP],
            item_trailer_len: 0,
        }
    }

    /// Reborrows request resources for one bounded operation.
    pub fn resources(&mut self) -> &mut ResourceContext<'control> {
        self.resources
    }

    /// Names the cooperative credit budget a straight-line decode/encode replenishes when its work meter reports
    /// `Pending`. Zero (the default) means no replenishment is configured.
    pub fn set_cooperative_credits(&mut self, credits: u32) {
        self.cooperative_credits = credits;
    }

    /// Copies a short facade suffix so the encoder can append it to its last staging write. A suffix longer than the
    /// inline buffer is left unset and the drive publishes it after encode.
    pub fn set_item_trailer(&mut self, bytes: &[u8]) {
        if bytes.len() > ITEM_TRAILER_CAP {
            self.item_trailer_len = 0;
            return;
        }
        self.item_trailer[..bytes.len()].copy_from_slice(bytes);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the guard above refuses anything past ITEM_TRAILER_CAP (8), so the length fits a u8"
        )]
        let len = bytes.len() as u8;
        self.item_trailer_len = len;
    }

    /// Bytes the encoder should fold into its last staging write, if any.
    #[must_use]
    pub fn item_trailer(&self) -> &[u8] {
        &self.item_trailer[..usize::from(self.item_trailer_len)]
    }

    /// Marks the trailer consumed so the drive does not publish it again.
    pub fn consume_item_trailer(&mut self) {
        self.item_trailer_len = 0;
    }

    /// Replenishes the work meter's cooperative entry budget and observes control — the straight-line replacement for
    /// a `Pending` yield. A codec calls this at its loop heads when [`jqf_resource::WorkAdmission::Pending`] reports
    /// the meter exhausted.
    ///
    /// # Errors
    ///
    /// Returns a control error when cancellation or the deadline is observed, or the typed memory-limit error when the
    /// ambient trip latch is set. A false replenish (no budget configured — zero or past the meter's validity law) is
    /// an internal contract violation: the drive set no budget yet the codec exhausted its meter.
    pub fn replenish_work(&mut self) -> Result<(), crate::CodecError> {
        let credits = self.cooperative_credits;
        match self.resources.try_begin_next_cooperative_entry(credits) {
            Ok(true) => Ok(()),
            Ok(false) => Err(crate::CodecError::new(
                crate::CodecFailureKind::InternalContractViolation {
                    contract: "codec exhausted its work meter without a configured cooperative budget",
                },
            )),
            Err(error) => Err(crate::CodecError::from(error)),
        }
    }
}

/// Stable terminal state shared by codec session wrappers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionTerminal {
    /// The session transferred all output and completed.
    Complete,
    /// The session ended with this stable failure classification.
    Failed(crate::CodecFailureKind),
    /// The session was explicitly aborted.
    Aborted,
}
