// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Centralized product identity constants for the PipeWire graph.
//!
//! All node, stream, group, and thread loop names must be referenced
//! exclusively from this module — never as inline string literals.

/// Name of the capture node (Virtual Sink). Visible in patchbays (qpwgraph, Helvum).
pub const PW_CAPTURE_NODE_NAME: &str = "NAM-Audio-Pipe-input";
/// Description of the capture node. Visible in patchbays.
pub const PW_CAPTURE_NODE_DESC: &str = "NAM-Audio-Pipe Input";
/// Name of the capture stream passed to the `StreamBox::new` constructor.
pub const PW_CAPTURE_STREAM_NAME: &str = "NAM-Audio-Pipe";

/// Name of the playback node. Visible in patchbays.
pub const PW_PLAYBACK_NODE_NAME: &str = "NAM-Audio-Pipe-playback";
/// Description of the playback node. Visible in patchbays.
pub const PW_PLAYBACK_NODE_DESC: &str = "NAM-Audio-Pipe Processed Output";
/// Name of the playback stream passed to the `StreamBox::new` constructor.
pub const PW_PLAYBACK_STREAM_NAME: &str = "NAM-Audio-Pipe-Output";

/// `node.group` — ensures both streams are scheduled by the same driver.
pub const PW_NODE_GROUP: &str = "nam-audio-pipe-dsp";
/// `node.link-group` — maintains both streams in the same link group.
pub const PW_LINK_GROUP: &str = "nam-audio-pipe-link-group";
/// Name of the PipeWire thread loop (internal, not visible to the user).
pub const PW_THREAD_LOOP_NAME: &str = "nam-audio-pipe-loop";
