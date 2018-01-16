/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use api::{BorderRadius, ClipMode, LayerPoint, LayerPointAu, LayerRect, LayerSize};
use app_units::Au;
use prim_store::EdgeAaSegmentMask;
use std::{cmp, usize};
use util::extract_inner_rect_safe;

bitflags! {
    pub struct ItemFlags: u8 {
        const X_ACTIVE = 0x1;
        const Y_ACTIVE = 0x2;
        const HAS_MASK = 0x4;
    }
}

// The segment builder outputs a list of these segments.
#[derive(Debug, PartialEq)]
pub struct Segment {
    pub rect: LayerRect,
    pub has_mask: bool,
    pub edge_flags: EdgeAaSegmentMask,
    pub region_x: usize,
    pub region_y: usize,
}

// The segment builder creates a list of x/y axis events
// that are used to build a segment list. Right now, we
// don't bother providing a list of *which* clip regions
// are active for a given segment. Instead, if there is
// any clip mask present in a segment, we will just end
// up drawing each of the masks to that segment clip.
// This is a fairly rare case, but we can detect this
// in the future and only apply clip masks that are
// relevant to each segment region.
// TODO(gw): Provide clip region info with each segment.
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd)]
enum EventKind {
    // Beginning of a clip (rounded) rect.
    BeginClip,
    // End of a clip (rounded) rect.
    EndClip,
    // Begin the next region in the primitive.
    BeginRegion,
}

// Events must be ordered such that when the coordinates
// of two events are the same, the end events are processed
// before the begin events. This ensures that we're able
// to detect which regions are active for a given segment.
impl Ord for EventKind {
    fn cmp(&self, other: &EventKind) -> cmp::Ordering {
        match (*self, *other) {
            (EventKind::BeginRegion, EventKind::BeginRegion) => {
                panic!("bug: regions must be non-overlapping")
            }
            (EventKind::EndClip, EventKind::BeginRegion) |
            (EventKind::BeginRegion, EventKind::BeginClip) => {
                cmp::Ordering::Less
            }
            (EventKind::BeginClip, EventKind::BeginRegion) |
            (EventKind::BeginRegion, EventKind::EndClip) => {
                cmp::Ordering::Greater
            }
            (EventKind::BeginClip, EventKind::BeginClip) |
            (EventKind::EndClip, EventKind::EndClip) => {
                cmp::Ordering::Equal
            }
            (EventKind::BeginClip, EventKind::EndClip) => {
                cmp::Ordering::Greater
            }
            (EventKind::EndClip, EventKind::BeginClip) => {
                cmp::Ordering::Less
            }
        }
    }
}

// A x/y event where we will create a vertex in the
// segment builder.
#[derive(Debug, Eq, PartialEq, PartialOrd)]
struct Event {
    value: Au,
    item_index: ItemIndex,
    kind: EventKind,
}

impl Ord for Event {
    fn cmp(&self, other: &Event) -> cmp::Ordering {
        self.value
            .cmp(&other.value)
            .then(self.kind.cmp(&other.kind))
    }
}

impl Event {
    fn begin(value: f32, index: usize) -> Event {
        Event {
            value: Au::from_f32_px(value),
            item_index: ItemIndex(index),
            kind: EventKind::BeginClip,
        }
    }

    fn end(value: f32, index: usize) -> Event {
        Event {
            value: Au::from_f32_px(value),
            item_index: ItemIndex(index),
            kind: EventKind::EndClip,
        }
    }

    fn region(value: f32) -> Event {
        Event {
            value: Au::from_f32_px(value),
            kind: EventKind::BeginRegion,
            item_index: ItemIndex(usize::MAX),
        }
    }

    fn update(
        &self,
        flag: ItemFlags,
        items: &mut [Item],
        region: &mut usize,
    ) {
        let is_active = match self.kind {
            EventKind::BeginClip => true,
            EventKind::EndClip => false,
            EventKind::BeginRegion => {
                *region += 1;
                return;
            }
        };

        items[self.item_index.0].flags.set(flag, is_active);
    }
}

// An item that provides some kind of clip region (either
// a clip in/out rect, or a mask region).
#[derive(Debug)]
struct Item {
    rect: LayerRect,
    mode: ClipMode,
    flags: ItemFlags,
}

impl Item {
    fn new(
        rect: LayerRect,
        mode: ClipMode,
        has_mask: bool,
    ) -> Item {
        let flags = if has_mask {
            ItemFlags::HAS_MASK
        } else {
            ItemFlags::empty()
        };

        Item {
            rect,
            mode,
            flags,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd)]
struct ItemIndex(usize);

// The main public interface to the segment module.
pub struct SegmentBuilder {
    items: Vec<Item>,
    inner_rect: Option<LayerRect>,
    bounding_rect: Option<LayerRect>,
}

impl SegmentBuilder {
    // Create a new segment builder, supplying the primitive
    // local rect and associated local clip rect.
    pub fn new(
        local_rect: LayerRect,
        inner_rect: Option<LayerRect>,
        local_clip_rect: LayerRect,
    ) -> SegmentBuilder {
        let mut builder = SegmentBuilder {
            items: Vec::new(),
            bounding_rect: Some(local_rect),
            inner_rect,
        };

        builder.push_rect(local_rect, None, ClipMode::Clip);
        builder.push_rect(local_clip_rect, None, ClipMode::Clip);

        builder
    }

    // Push some kind of clipping region into the segment builder.
    // If radius is None, it's a simple rect.
    pub fn push_rect(
        &mut self,
        rect: LayerRect,
        radius: Option<BorderRadius>,
        mode: ClipMode,
    ) {
        // Keep track of a minimal bounding rect for the set of
        // segments that will be generated.
        if mode == ClipMode::Clip {
            self.bounding_rect = self.bounding_rect.and_then(|bounding_rect| {
                bounding_rect.intersection(&rect)
            });
        }

        match radius {
            Some(radius) => {
                // For a rounded rect, try to create a nine-patch where there
                // is a clip item for each corner, inner and edge region.
                match extract_inner_rect_safe(&rect, &radius) {
                    Some(inner) => {
                        let p0 = rect.origin;
                        let p1 = inner.origin;
                        let p2 = inner.bottom_right();
                        let p3 = rect.bottom_right();

                        let corner_segments = &[
                            LayerRect::new(
                                LayerPoint::new(p0.x, p0.y),
                                LayerSize::new(p1.x - p0.x, p1.y - p0.y),
                            ),
                            LayerRect::new(
                                LayerPoint::new(p2.x, p0.y),
                                LayerSize::new(p3.x - p2.x, p1.y - p0.y),
                            ),
                            LayerRect::new(
                                LayerPoint::new(p2.x, p2.y),
                                LayerSize::new(p3.x - p2.x, p3.y - p2.y),
                            ),
                            LayerRect::new(
                                LayerPoint::new(p0.x, p2.y),
                                LayerSize::new(p1.x - p0.x, p3.y - p2.y),
                            ),
                        ];

                        for segment in corner_segments {
                            self.items.push(Item::new(
                                *segment,
                                mode,
                                true
                            ));
                        }

                        let other_segments = &[
                            LayerRect::new(
                                LayerPoint::new(p1.x, p0.y),
                                LayerSize::new(p2.x - p1.x, p1.y - p0.y),
                            ),
                            LayerRect::new(
                                LayerPoint::new(p2.x, p1.y),
                                LayerSize::new(p3.x - p2.x, p2.y - p1.y),
                            ),
                            LayerRect::new(
                                LayerPoint::new(p1.x, p2.y),
                                LayerSize::new(p2.x - p1.x, p3.y - p2.y),
                            ),
                            LayerRect::new(
                                LayerPoint::new(p0.x, p1.y),
                                LayerSize::new(p1.x - p0.x, p2.y - p1.y),
                            ),
                            LayerRect::new(
                                LayerPoint::new(p1.x, p1.y),
                                LayerSize::new(p2.x - p1.x, p2.y - p1.y),
                            ),
                        ];

                        for segment in other_segments {
                            self.items.push(Item::new(
                                *segment,
                                mode,
                                false,
                            ));
                        }
                    }
                    None => {
                        // If we get here, we could not extract an inner rectangle
                        // for this clip region. This can occur in cases such as
                        // a rounded rect where the top-left and bottom-left radii
                        // result in overlapping rects. In that case, just create
                        // a single clip region for the entire rounded rect.
                        self.items.push(Item::new(
                            rect,
                            mode,
                            true,
                        ))
                    }
                }
            }
            None => {
                // For a simple rect, just create one clipping item.
                self.items.push(Item::new(
                    rect,
                    mode,
                    false,
                ))
            }
        }
    }

    // Consume this segment builder and produce a list of segments.
    pub fn build<F>(self, mut f: F) where F: FnMut(&Segment) {
        let bounding_rect = match self.bounding_rect {
            Some(bounding_rect) => bounding_rect,
            None => return,
        };

        // First, filter out any items that don't intersect
        // with the visible bounding rect.
        let mut items: Vec<Item> = self.items
            .into_iter()
            .filter(|item| item.rect.intersects(&bounding_rect))
            .collect();

        // Create events for each item
        let mut x_events = Vec::new();
        let mut y_events = Vec::new();

        for (item_index, item) in items.iter().enumerate() {
            let p0 = item.rect.origin;
            let p1 = item.rect.bottom_right();

            x_events.push(Event::begin(p0.x, item_index));
            x_events.push(Event::end(p1.x, item_index));
            y_events.push(Event::begin(p0.y, item_index));
            y_events.push(Event::end(p1.y, item_index));
        }

        // Add the region events, if provided.
        if let Some(inner_rect) = self.inner_rect {
            x_events.push(Event::region(inner_rect.origin.x));
            x_events.push(Event::region(inner_rect.origin.x + inner_rect.size.width));

            y_events.push(Event::region(inner_rect.origin.y));
            y_events.push(Event::region(inner_rect.origin.y + inner_rect.size.height));
        }

        // Get the minimal bounding rect in app units. We will
        // work in fixed point in order to avoid float precision
        // error while handling events.
        let p0 = LayerPointAu::new(
            Au::from_f32_px(bounding_rect.origin.x),
            Au::from_f32_px(bounding_rect.origin.y),
        );

        let p1 = LayerPointAu::new(
            Au::from_f32_px(bounding_rect.origin.x + bounding_rect.size.width),
            Au::from_f32_px(bounding_rect.origin.y + bounding_rect.size.height),
        );

        // Sort the events in ascending order.
        x_events.sort();
        y_events.sort();

        // Generate segments from the event lists, by sweeping the y-axis
        // and then the x-axis for each event. This can generate a significant
        // number of segments, but most importantly, it ensures that there are
        // no t-junctions in the generated segments. It's probably possible
        // to come up with more efficient segmentation algorithms, at least
        // for simple / common cases.

        // Each coordinate is clamped to the bounds of the minimal
        // bounding rect. This ensures that we don't generate segments
        // outside that bounding rect, but does allow correctly handling
        // clips where the clip region starts outside the minimal
        // rect but still intersects with it.

        let mut prev_y = clamp(p0.y, y_events[0].value, p1.y);
        let mut region_y = 0;
        let mut segments = Vec::new();
        let mut x_count = 0;
        let mut y_count = 0;

        for ey in &y_events {
            let cur_y = clamp(p0.y, ey.value, p1.y);

            if cur_y != prev_y {
                let mut prev_x = clamp(p0.x, x_events[0].value, p1.x);
                let mut region_x = 0;

                for ex in &x_events {
                    let cur_x = clamp(p0.x, ex.value, p1.x);

                    if cur_x != prev_x {
                        segments.push(emit_segment_if_needed(
                            prev_x,
                            prev_y,
                            cur_x,
                            cur_y,
                            region_x,
                            region_y,
                            &items,
                        ));

                        prev_x = cur_x;
                        if y_count == 0 {
                            x_count += 1;
                        }
                    }

                    ex.update(
                        ItemFlags::X_ACTIVE,
                        &mut items,
                        &mut region_x,
                    );
                }

                prev_y = cur_y;
                y_count += 1;
            }

            ey.update(
                ItemFlags::Y_ACTIVE,
                &mut items,
                &mut region_y,
            );
        }

        // Run user supplied closure for each valid segment.
        debug_assert_eq!(segments.len(), x_count * y_count);
        for y in 0 .. y_count {
            for x in 0 .. x_count {
                let mut edge_flags = EdgeAaSegmentMask::empty();

                if x == 0 || segments[y * x_count + x - 1].is_none() {
                    edge_flags |= EdgeAaSegmentMask::LEFT;
                }
                if x == x_count-1 || segments[y * x_count + x + 1].is_none() {
                    edge_flags |= EdgeAaSegmentMask::RIGHT;
                }
                if y == 0 || segments[(y-1) * x_count + x].is_none() {
                    edge_flags |= EdgeAaSegmentMask::TOP;
                }
                if y == y_count-1 || segments[(y+1) * x_count + x].is_none() {
                    edge_flags |= EdgeAaSegmentMask::BOTTOM;
                }

                if let Some(ref mut segment) = segments[y * x_count + x] {
                    segment.edge_flags = edge_flags;
                    f(segment);
                }
            }
        }
    }
}

fn clamp(low: Au, value: Au, high: Au) -> Au {
    value.max(low).min(high)
}

fn emit_segment_if_needed(
    x0: Au,
    y0: Au,
    x1: Au,
    y1: Au,
    region_x: usize,
    region_y: usize,
    items: &[Item],
) -> Option<Segment> {
    debug_assert!(x1 > x0);
    debug_assert!(y1 > y0);

    // TODO(gw): Don't scan the whole list of items for
    //           each segment rect. Store active list
    //           in a hash set or similar if this ever
    //           shows up in a profile.
    let mut has_clip_mask = false;

    for item in items {
        if item.flags.contains(ItemFlags::X_ACTIVE | ItemFlags::Y_ACTIVE) {
            has_clip_mask |= item.flags.contains(ItemFlags::HAS_MASK);

            if item.mode == ClipMode::ClipOut && !item.flags.contains(ItemFlags::HAS_MASK) {
                return None;
            }
        }
    }

    let segment_rect = LayerRect::new(
        LayerPoint::new(
            x0.to_f32_px(),
            y0.to_f32_px(),
        ),
        LayerSize::new(
            (x1 - x0).to_f32_px(),
            (y1 - y0).to_f32_px(),
        ),
    );

    Some(Segment {
        rect: segment_rect,
        has_mask: has_clip_mask,
        edge_flags: EdgeAaSegmentMask::empty(),
        region_x,
        region_y,
    })
}

#[cfg(test)]
mod test {
    use api::{BorderRadius, ClipMode, LayerPoint, LayerRect, LayerSize};
    use prim_store::EdgeAaSegmentMask;
    use super::{Segment, SegmentBuilder};
    use std::cmp;

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> LayerRect {
        LayerRect::new(
            LayerPoint::new(x0, y0),
            LayerSize::new(x1-x0, y1-y0),
        )
    }

    fn seg(
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        has_mask: bool,
        edge_flags: Option<EdgeAaSegmentMask>,
    ) -> Segment {
        seg_region(x0, y0, x1, y1, 0, 0, has_mask, edge_flags)
    }

    fn seg_region(
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        region_x: usize,
        region_y: usize,
        has_mask: bool,
        edge_flags: Option<EdgeAaSegmentMask>,
    ) -> Segment {
        Segment {
            rect: LayerRect::new(
                LayerPoint::new(x0, y0),
                LayerSize::new(x1-x0, y1-y0),
            ),
            has_mask,
            edge_flags: edge_flags.unwrap_or(EdgeAaSegmentMask::empty()),
            region_x,
            region_y,
        }
    }

    fn segment_sorter(s0: &Segment, s1: &Segment) -> cmp::Ordering {
        let r0 = &s0.rect;
        let r1 = &s1.rect;

        (
            (r0.origin.x, r0.origin.y, r0.size.width, r0.size.height)
        ).partial_cmp(&
            (r1.origin.x, r1.origin.y, r1.size.width, r1.size.height)
        ).unwrap()
    }

    fn seg_test(
        local_rect: LayerRect,
        inner_rect: Option<LayerRect>,
        local_clip_rect: LayerRect,
        clips: &[(LayerRect, Option<BorderRadius>, ClipMode)],
        expected_segments: &mut [Segment]
    ) {
        let mut sb = SegmentBuilder::new(
            local_rect,
            inner_rect,
            local_clip_rect,
        );
        let mut segments = Vec::new();
        for &(rect, radius, mode) in clips {
            sb.push_rect(rect, radius, mode);
        }
        sb.build(|segment| {
            segments.push(Segment {
                ..*segment
            });
        });
        segments.sort_by(segment_sorter);
        expected_segments.sort_by(segment_sorter);
        assert_eq!(
            segments.len(),
            expected_segments.len(),
            "segments\n{:?}\nexpected\n{:?}\n",
            segments,
            expected_segments
        );
        for (segment, expected) in segments.iter().zip(expected_segments.iter()) {
            assert_eq!(segment, expected);
        }
    }

    #[test]
    fn segment_empty() {
        seg_test(
            rect(0.0, 0.0, 0.0, 0.0),
            None,
            rect(0.0, 0.0, 0.0, 0.0),
            &[],
            &mut [],
        );
    }

    #[test]
    fn segment_single() {
        seg_test(
            rect(10.0, 20.0, 30.0, 40.0),
            None,
            rect(10.0, 20.0, 30.0, 40.0),
            &[],
            &mut [
                seg(10.0, 20.0, 30.0, 40.0, false,
                    Some(EdgeAaSegmentMask::LEFT |
                         EdgeAaSegmentMask::TOP |
                         EdgeAaSegmentMask::RIGHT |
                         EdgeAaSegmentMask::BOTTOM
                    )
                ),
            ],
        );
    }

    #[test]
    fn segment_single_clip() {
        seg_test(
            rect(10.0, 20.0, 30.0, 40.0),
            None,
            rect(10.0, 20.0, 25.0, 35.0),
            &[],
            &mut [
                seg(10.0, 20.0, 25.0, 35.0, false,
                    Some(EdgeAaSegmentMask::LEFT |
                         EdgeAaSegmentMask::TOP |
                         EdgeAaSegmentMask::RIGHT |
                         EdgeAaSegmentMask::BOTTOM
                    )
                ),
            ],
        );
    }

    #[test]
    fn segment_inner_clip() {
        seg_test(
            rect(10.0, 20.0, 30.0, 40.0),
            None,
            rect(15.0, 25.0, 25.0, 35.0),
            &[],
            &mut [
                seg(15.0, 25.0, 25.0, 35.0, false,
                    Some(EdgeAaSegmentMask::LEFT |
                         EdgeAaSegmentMask::TOP |
                         EdgeAaSegmentMask::RIGHT |
                         EdgeAaSegmentMask::BOTTOM
                    )
                ),
            ],
        );
    }

    #[test]
    fn segment_outer_clip() {
        seg_test(
            rect(15.0, 25.0, 25.0, 35.0),
            None,
            rect(10.0, 20.0, 30.0, 40.0),
            &[],
            &mut [
                seg(15.0, 25.0, 25.0, 35.0, false,
                    Some(EdgeAaSegmentMask::LEFT |
                         EdgeAaSegmentMask::TOP |
                         EdgeAaSegmentMask::RIGHT |
                         EdgeAaSegmentMask::BOTTOM
                    )
                ),
            ],
        );
    }

    #[test]
    fn segment_clip_int() {
        seg_test(
            rect(10.0, 20.0, 30.0, 40.0),
            None,
            rect(20.0, 10.0, 40.0, 30.0),
            &[],
            &mut [
                seg(20.0, 20.0, 30.0, 30.0, false,
                    Some(EdgeAaSegmentMask::LEFT |
                         EdgeAaSegmentMask::TOP |
                         EdgeAaSegmentMask::RIGHT |
                         EdgeAaSegmentMask::BOTTOM
                    )
                ),
            ],
        );
    }

    #[test]
    fn segment_clip_disjoint() {
        seg_test(
            rect(10.0, 20.0, 30.0, 40.0),
            None,
            rect(30.0, 20.0, 50.0, 40.0),
            &[],
            &mut [],
        );
    }

    #[test]
    fn segment_clips() {
        seg_test(
            rect(0.0, 0.0, 100.0, 100.0),
            None,
            rect(-1000.0, -1000.0, 1000.0, 1000.0),
            &[
                (rect(20.0, 20.0, 40.0, 40.0), None, ClipMode::Clip),
                (rect(40.0, 20.0, 60.0, 40.0), None, ClipMode::Clip),
            ],
            &mut [
            ],
        );
    }

    #[test]
    fn segment_rounded_clip() {
        seg_test(
            rect(0.0, 0.0, 100.0, 100.0),
            None,
            rect(-1000.0, -1000.0, 1000.0, 1000.0),
            &[
                (rect(20.0, 20.0, 60.0, 60.0), Some(BorderRadius::uniform(10.0)), ClipMode::Clip),
            ],
            &mut [
                // corners
                seg(20.0, 20.0, 30.0, 30.0, true, Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::TOP)),
                seg(20.0, 50.0, 30.0, 60.0, true, Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::BOTTOM)),
                seg(50.0, 20.0, 60.0, 30.0, true, Some(EdgeAaSegmentMask::RIGHT | EdgeAaSegmentMask::TOP)),
                seg(50.0, 50.0, 60.0, 60.0, true, Some(EdgeAaSegmentMask::RIGHT | EdgeAaSegmentMask::BOTTOM)),

                // inner
                seg(30.0, 30.0, 50.0, 50.0, false, None),

                // edges
                seg(30.0, 20.0, 50.0, 30.0, false, Some(EdgeAaSegmentMask::TOP)),
                seg(30.0, 50.0, 50.0, 60.0, false, Some(EdgeAaSegmentMask::BOTTOM)),
                seg(20.0, 30.0, 30.0, 50.0, false, Some(EdgeAaSegmentMask::LEFT)),
                seg(50.0, 30.0, 60.0, 50.0, false, Some(EdgeAaSegmentMask::RIGHT)),
            ],
        );
    }

    #[test]
    fn segment_clip_out() {
        seg_test(
            rect(0.0, 0.0, 100.0, 100.0),
            None,
            rect(-1000.0, -1000.0, 2000.0, 2000.0),
            &[
                (rect(20.0, 20.0, 60.0, 60.0), None, ClipMode::ClipOut),
            ],
            &mut [
                seg(0.0, 0.0, 20.0, 20.0, false, Some(EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::LEFT)),
                seg(20.0, 0.0, 60.0, 20.0, false, Some(EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::BOTTOM)),
                seg(60.0, 0.0, 100.0, 20.0, false, Some(EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::RIGHT)),

                seg(0.0, 20.0, 20.0, 60.0, false, Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::RIGHT)),
                seg(60.0, 20.0, 100.0, 60.0, false, Some(EdgeAaSegmentMask::RIGHT | EdgeAaSegmentMask::LEFT)),

                seg(0.0, 60.0, 20.0, 100.0, false, Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::BOTTOM)),
                seg(20.0, 60.0, 60.0, 100.0, false, Some(EdgeAaSegmentMask::BOTTOM | EdgeAaSegmentMask::TOP)),
                seg(60.0, 60.0, 100.0, 100.0, false, Some(EdgeAaSegmentMask::BOTTOM | EdgeAaSegmentMask::RIGHT)),
            ],
        );
    }

    #[test]
    fn segment_rounded_clip_out() {
        seg_test(
            rect(0.0, 0.0, 100.0, 100.0),
            None,
            rect(-1000.0, -1000.0, 2000.0, 2000.0),
            &[
                (rect(20.0, 20.0, 60.0, 60.0), Some(BorderRadius::uniform(10.0)), ClipMode::ClipOut),
            ],
            &mut [
                // top row
                seg(0.0, 0.0, 20.0, 20.0, false, Some(EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::LEFT)),
                seg(20.0, 0.0, 30.0, 20.0, false, Some(EdgeAaSegmentMask::TOP)),
                seg(30.0, 0.0, 50.0, 20.0, false, Some(EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::BOTTOM)),
                seg(50.0, 0.0, 60.0, 20.0, false, Some(EdgeAaSegmentMask::TOP)),
                seg(60.0, 0.0, 100.0, 20.0, false, Some(EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::RIGHT)),

                // left
                seg(0.0, 20.0, 20.0, 30.0, false, Some(EdgeAaSegmentMask::LEFT)),
                seg(0.0, 30.0, 20.0, 50.0, false, Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::RIGHT)),
                seg(0.0, 50.0, 20.0, 60.0, false, Some(EdgeAaSegmentMask::LEFT)),

                // right
                seg(60.0, 20.0, 100.0, 30.0, false, Some(EdgeAaSegmentMask::RIGHT)),
                seg(60.0, 30.0, 100.0, 50.0, false, Some(EdgeAaSegmentMask::RIGHT | EdgeAaSegmentMask::LEFT)),
                seg(60.0, 50.0, 100.0, 60.0, false, Some(EdgeAaSegmentMask::RIGHT)),

                // bottom row
                seg(0.0, 60.0, 20.0, 100.0, false, Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::BOTTOM)),
                seg(20.0, 60.0, 30.0, 100.0, false, Some(EdgeAaSegmentMask::BOTTOM)),
                seg(30.0, 60.0, 50.0, 100.0, false, Some(EdgeAaSegmentMask::BOTTOM | EdgeAaSegmentMask::TOP)),
                seg(50.0, 60.0, 60.0, 100.0, false, Some(EdgeAaSegmentMask::BOTTOM)),
                seg(60.0, 60.0, 100.0, 100.0, false, Some(EdgeAaSegmentMask::RIGHT | EdgeAaSegmentMask::BOTTOM)),

                // inner corners
                seg(20.0, 20.0, 30.0, 30.0, true, Some(EdgeAaSegmentMask::RIGHT | EdgeAaSegmentMask::BOTTOM)),
                seg(20.0, 50.0, 30.0, 60.0, true, Some(EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::RIGHT)),
                seg(50.0, 20.0, 60.0, 30.0, true, Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::BOTTOM)),
                seg(50.0, 50.0, 60.0, 60.0, true, Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::TOP)),
            ],
        );
    }

    #[test]
    fn segment_clip_in_clip_out() {
        seg_test(
            rect(0.0, 0.0, 100.0, 100.0),
            None,
            rect(-1000.0, -1000.0, 2000.0, 2000.0),
            &[
                (rect(20.0, 20.0, 60.0, 60.0), None, ClipMode::Clip),
                (rect(50.0, 50.0, 80.0, 80.0), None, ClipMode::ClipOut),
            ],
            &mut [
                seg(20.0, 20.0, 50.0, 50.0, false, Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::TOP)),
                seg(50.0, 20.0, 60.0, 50.0, false, Some(EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::RIGHT | EdgeAaSegmentMask::BOTTOM)),
                seg(20.0, 50.0, 50.0, 60.0, false, Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::BOTTOM | EdgeAaSegmentMask::RIGHT)),
            ],
        );
    }

    #[test]
    fn segment_rounded_clip_overlap() {
        seg_test(
            rect(0.0, 0.0, 100.0, 100.0),
            None,
            rect(0.0, 0.0, 100.0, 100.0),
            &[
                (rect(0.0, 0.0, 10.0, 10.0), None, ClipMode::ClipOut),
                (rect(0.0, 0.0, 100.0, 100.0), Some(BorderRadius::uniform(10.0)), ClipMode::Clip),
            ],
            &mut [
                // corners
                seg(0.0, 90.0, 10.0, 100.0, true, Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::BOTTOM)),
                seg(90.0, 0.0, 100.0, 10.0, true, Some(EdgeAaSegmentMask::RIGHT | EdgeAaSegmentMask::TOP)),
                seg(90.0, 90.0, 100.0, 100.0, true, Some(EdgeAaSegmentMask::RIGHT | EdgeAaSegmentMask::BOTTOM)),

                // inner
                seg(10.0, 10.0, 90.0, 90.0, false, None),

                // edges
                seg(10.0, 0.0, 90.0, 10.0, false, Some(EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::LEFT)),
                seg(10.0, 90.0, 90.0, 100.0, false, Some(EdgeAaSegmentMask::BOTTOM)),
                seg(0.0, 10.0, 10.0, 90.0, false, Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::TOP)),
                seg(90.0, 10.0, 100.0, 90.0, false, Some(EdgeAaSegmentMask::RIGHT)),
            ],
        );
    }

    #[test]
    fn segment_rounded_clip_overlap_reverse() {
        seg_test(
            rect(0.0, 0.0, 100.0, 100.0),
            None,
            rect(0.0, 0.0, 100.0, 100.0),
            &[
                (rect(10.0, 10.0, 90.0, 90.0), None, ClipMode::Clip),
                (rect(0.0, 0.0, 100.0, 100.0), Some(BorderRadius::uniform(10.0)), ClipMode::Clip),
            ],
            &mut [
                seg(10.0, 10.0, 90.0, 90.0, false,
                    Some(EdgeAaSegmentMask::LEFT |
                         EdgeAaSegmentMask::TOP |
                         EdgeAaSegmentMask::RIGHT |
                         EdgeAaSegmentMask::BOTTOM
                    )
                ),
            ],
        );
    }

    #[test]
    fn segment_clip_in_clip_out_overlap() {
        seg_test(
            rect(0.0, 0.0, 100.0, 100.0),
            None,
            rect(0.0, 0.0, 100.0, 100.0),
            &[
                (rect(10.0, 10.0, 90.0, 90.0), None, ClipMode::Clip),
                (rect(10.0, 10.0, 90.0, 90.0), None, ClipMode::ClipOut),
            ],
            &mut [
            ],
        );
    }

    #[test]
    fn segment_event_order() {
        seg_test(
            rect(0.0, 0.0, 100.0, 100.0),
            None,
            rect(0.0, 0.0, 100.0, 100.0),
            &[
                (rect(0.0, 0.0, 100.0, 90.0), None, ClipMode::ClipOut),
            ],
            &mut [
                seg(0.0, 90.0, 100.0, 100.0, false, Some(
                    EdgeAaSegmentMask::LEFT |
                    EdgeAaSegmentMask::RIGHT |
                    EdgeAaSegmentMask::BOTTOM |
                    EdgeAaSegmentMask::TOP
                )),
            ],
        );
    }

    #[test]
    fn segment_region_simple() {
        seg_test(
            rect(0.0, 0.0, 100.0, 100.0),
            Some(rect(20.0, 40.0, 60.0, 80.0)),
            rect(0.0, 0.0, 100.0, 100.0),
            &[
            ],
            &mut [
                seg_region(
                    0.0, 0.0,
                    20.0, 40.0,
                    0, 0,
                    false,
                    Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::TOP)
                ),

                seg_region(
                    20.0, 0.0,
                    60.0, 40.0,
                    1, 0,
                    false,
                    Some(EdgeAaSegmentMask::TOP)
                ),

                seg_region(
                    60.0, 0.0,
                    100.0, 40.0,
                    2, 0,
                    false,
                    Some(EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::RIGHT)
                ),

                seg_region(
                    0.0, 40.0,
                    20.0, 80.0,
                    0, 1,
                    false,
                    Some(EdgeAaSegmentMask::LEFT)
                ),

                seg_region(
                    20.0, 40.0,
                    60.0, 80.0,
                    1, 1,
                    false,
                    None,
                ),

                seg_region(
                    60.0, 40.0,
                    100.0, 80.0,
                    2, 1,
                    false,
                    Some(EdgeAaSegmentMask::RIGHT)
                ),

                seg_region(
                    0.0, 80.0,
                    20.0, 100.0,
                    0, 2,
                    false,
                    Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::BOTTOM)
                ),

                seg_region(
                    20.0, 80.0,
                    60.0, 100.0,
                    1, 2,
                    false,
                    Some(EdgeAaSegmentMask::BOTTOM),
                ),

                seg_region(
                    60.0, 80.0,
                    100.0, 100.0,
                    2, 2,
                    false,
                    Some(EdgeAaSegmentMask::RIGHT | EdgeAaSegmentMask::BOTTOM)
                ),

            ],
        );
    }

    #[test]
    fn segment_region_clip() {
        seg_test(
            rect(0.0, 0.0, 100.0, 100.0),
            Some(rect(20.0, 40.0, 60.0, 80.0)),
            rect(0.0, 0.0, 100.0, 100.0),
            &[
                (rect(0.0, 0.0, 100.0, 90.0), None, ClipMode::ClipOut),
            ],
            &mut [
                seg_region(
                    0.0, 90.0,
                    20.0, 100.0,
                    0, 2,
                    false,
                    Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::BOTTOM | EdgeAaSegmentMask::TOP)
                ),

                seg_region(
                    20.0, 90.0,
                    60.0, 100.0,
                    1, 2,
                    false,
                    Some(EdgeAaSegmentMask::BOTTOM | EdgeAaSegmentMask::TOP),
                ),

                seg_region(
                    60.0, 90.0,
                    100.0, 100.0,
                    2, 2,
                    false,
                    Some(EdgeAaSegmentMask::RIGHT | EdgeAaSegmentMask::BOTTOM | EdgeAaSegmentMask::TOP)
                ),

            ],
        );
    }

    #[test]
    fn segment_region_clip2() {
        seg_test(
            rect(0.0, 0.0, 100.0, 100.0),
            Some(rect(20.0, 20.0, 80.0, 80.0)),
            rect(0.0, 0.0, 100.0, 100.0),
            &[
                (rect(20.0, 20.0, 100.0, 100.0), None, ClipMode::ClipOut),
            ],
            &mut [
                seg_region(
                    0.0, 0.0,
                    20.0, 20.0,
                    0, 0,
                    false,
                    Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::TOP)
                ),

                seg_region(
                    20.0, 0.0,
                    80.0, 20.0,
                    1, 0,
                    false,
                    Some(EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::BOTTOM),
                ),

                seg_region(
                    80.0, 0.0,
                    100.0, 20.0,
                    2, 0,
                    false,
                    Some(EdgeAaSegmentMask::RIGHT | EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::BOTTOM)
                ),

                seg_region(
                    0.0, 20.0,
                    20.0, 80.0,
                    0, 1,
                    false,
                    Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::RIGHT)
                ),

                seg_region(
                    0.0, 80.0,
                    20.0, 100.0,
                    0, 2,
                    false,
                    Some(EdgeAaSegmentMask::LEFT | EdgeAaSegmentMask::BOTTOM | EdgeAaSegmentMask::RIGHT)
                ),
            ],
        );
    }

    #[test]
    fn segment_region_clip3() {
        seg_test(
            rect(0.0, 0.0, 100.0, 100.0),
            Some(rect(20.0, 20.0, 80.0, 80.0)),
            rect(0.0, 0.0, 100.0, 100.0),
            &[
                (rect(10.0, 10.0, 30.0, 30.0), None, ClipMode::Clip),
            ],
            &mut [
                seg_region(
                    10.0, 10.0,
                    20.0, 20.0,
                    0, 0,
                    false,
                    Some(EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::LEFT),
                ),

                seg_region(
                    20.0, 10.0,
                    30.0, 20.0,
                    1, 0,
                    false,
                    Some(EdgeAaSegmentMask::TOP | EdgeAaSegmentMask::RIGHT),
                ),

                seg_region(
                    10.0, 20.0,
                    20.0, 30.0,
                    0, 1,
                    false,
                    Some(EdgeAaSegmentMask::BOTTOM | EdgeAaSegmentMask::LEFT),
                ),

                seg_region(
                    20.0, 20.0,
                    30.0, 30.0,
                    1, 1,
                    false,
                    Some(EdgeAaSegmentMask::BOTTOM | EdgeAaSegmentMask::RIGHT),
                ),
            ],
        );
    }
}
