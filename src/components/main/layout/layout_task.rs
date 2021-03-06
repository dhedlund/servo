/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! The layout task. Performs layout on the DOM, builds display lists and sends them to be
/// rendered.

use css::matching::MatchMethods;
use css::select::new_stylist;
use css::node_style::StyledNode;
use layout::construct::{FlowConstructionResult, FlowConstructor, NoConstructionResult};
use layout::context::LayoutContext;
use layout::display_list_builder::{DisplayListBuilder, ToGfxColor};
use layout::extra::LayoutAuxMethods;
use layout::flow::{Flow, ImmutableFlowUtils, MutableFlowUtils, PreorderFlowTraversal};
use layout::flow::{PostorderFlowTraversal};
use layout::flow;
use layout::incremental::{RestyleDamage};
use layout::util::{LayoutData, LayoutDataAccess};

use extra::arc::{Arc, RWArc, MutexArc};
use geom::point::Point2D;
use geom::rect::Rect;
use geom::size::Size2D;
use gfx::display_list::{ClipDisplayItemClass, DisplayItem, DisplayItemIterator, DisplayList};
use gfx::font_context::FontContext;
use gfx::opts::Opts;
use gfx::render_task::{RenderMsg, RenderChan, RenderLayer};
use gfx::{render_task, color};
use script::dom::event::ReflowEvent;
use script::dom::node::{AbstractNode, LayoutDataRef, LayoutView, ElementNodeTypeId};
use script::dom::element::{HTMLBodyElementTypeId, HTMLHtmlElementTypeId};
use script::layout_interface::{AddStylesheetMsg, ContentBoxQuery};
use script::layout_interface::{ContentBoxesQuery, ContentBoxesResponse, ExitNowMsg, LayoutQuery};
use script::layout_interface::{HitTestQuery, ContentBoxResponse, HitTestResponse};
use script::layout_interface::{ContentChangedDocumentDamage, Msg, PrepareToExitMsg};
use script::layout_interface::{QueryMsg, ReapLayoutDataMsg, Reflow, ReflowDocumentDamage};
use script::layout_interface::{ReflowForDisplay, ReflowMsg};
use script::script_task::{ReflowCompleteMsg, ScriptChan, SendEventMsg};
use servo_msg::constellation_msg::{ConstellationChan, PipelineId};
use servo_net::image_cache_task::{ImageCacheTask, ImageResponseMsg};
use servo_net::local_image_cache::{ImageResponder, LocalImageCache};
use servo_util::geometry::Au;
use servo_util::time::{ProfilerChan, profile};
use servo_util::time;
use servo_util::tree::TreeNodeRef;
use std::cast::transmute;
use std::cast;
use std::cell::Cell;
use std::comm::Port;
use std::task;
use std::util;
use style::AuthorOrigin;
use style::Stylesheet;
use style::Stylist;

/// Information needed by the layout task.
struct LayoutTask {
    /// The ID of the pipeline that we belong to.
    id: PipelineId,

    /// The port on which we receive messages.
    port: Port<Msg>,

    /// The channel on which messages can be sent to the constellation.
    constellation_chan: ConstellationChan,

    /// The channel on which messages can be sent to the script task.
    script_chan: ScriptChan,

    /// The channel on which messages can be sent to the painting task.
    render_chan: RenderChan<AbstractNode<()>>,

    /// The channel on which messages can be sent to the image cache.
    image_cache_task: ImageCacheTask,

    /// The local image cache.
    local_image_cache: MutexArc<LocalImageCache>,

    /// The size of the viewport.
    screen_size: Option<Size2D<Au>>,

    /// A cached display list.
    display_list: Option<Arc<DisplayList<AbstractNode<()>>>>,

    stylist: RWArc<Stylist>,

    /// The channel on which messages can be sent to the profiler.
    profiler_chan: ProfilerChan,

    opts: Opts
}

/// The damage computation traversal.
#[deriving(Clone)]
struct ComputeDamageTraversal;

impl PostorderFlowTraversal for ComputeDamageTraversal {
    #[inline]
    fn process(&mut self, flow: &mut Flow) -> bool {
        let mut damage = flow::base(flow).restyle_damage;
        for child in flow::child_iter(flow) {
            damage.union_in_place(flow::base(*child).restyle_damage.propagate_up())
        }
        flow::mut_base(flow).restyle_damage = damage;
        true
    }
}

/// Propagates restyle damage up and down the tree as appropriate.
///
/// FIXME(pcwalton): Merge this with flow tree building and/or other traversals.
struct PropagateDamageTraversal {
    all_style_damage: bool,
}

impl PreorderFlowTraversal for PropagateDamageTraversal {
    #[inline]
    fn process(&mut self, flow: &mut Flow) -> bool {
        if self.all_style_damage {
            flow::mut_base(flow).restyle_damage.union_in_place(RestyleDamage::all())
        }
        debug!("restyle damage = {:?}", flow::base(flow).restyle_damage);

        let prop = flow::base(flow).restyle_damage.propagate_down();
        if prop.is_nonempty() {
            for kid_ctx in flow::child_iter(flow) {
                flow::mut_base(*kid_ctx).restyle_damage.union_in_place(prop)
            }
        }
        true
    }
}

/// The bubble-widths traversal, the first part of layout computation. This computes preferred
/// and intrinsic widths and bubbles them up the tree.
struct BubbleWidthsTraversal<'self>(&'self mut LayoutContext);

impl<'self> PostorderFlowTraversal for BubbleWidthsTraversal<'self> {
    #[inline]
    fn process(&mut self, flow: &mut Flow) -> bool {
        flow.bubble_widths(**self);
        true
    }

    // FIXME: We can't prune until we start reusing flows
    /*
    #[inline]
    fn should_prune(&mut self, flow: &mut Flow) -> bool {
        flow::mut_base(flow).restyle_damage.lacks(BubbleWidths)
    }
    */
}

/// The assign-widths traversal. In Gecko this corresponds to `Reflow`.
struct AssignWidthsTraversal<'self>(&'self mut LayoutContext);

impl<'self> PreorderFlowTraversal for AssignWidthsTraversal<'self> {
    #[inline]
    fn process(&mut self, flow: &mut Flow) -> bool {
        flow.assign_widths(**self);
        true
    }
}

/// The assign-heights-and-store-overflow traversal, the last (and most expensive) part of layout
/// computation. Determines the final heights for all layout objects, computes positions, and
/// computes overflow regions. In Gecko this corresponds to `FinishAndStoreOverflow`.
struct AssignHeightsAndStoreOverflowTraversal<'self>(&'self mut LayoutContext);

impl<'self> PostorderFlowTraversal for AssignHeightsAndStoreOverflowTraversal<'self> {
    #[inline]
    fn process(&mut self, flow: &mut Flow) -> bool {
        flow.assign_height(**self);
        flow.store_overflow(**self);
        true
    }

    #[inline]
    fn should_process(&mut self, flow: &mut Flow) -> bool {
        !flow::base(flow).flags.inorder()
    }
}

struct LayoutImageResponder {
    id: PipelineId,
    script_chan: ScriptChan,
}

impl ImageResponder for LayoutImageResponder {
    fn respond(&self) -> ~fn(ImageResponseMsg) {
        let id = self.id.clone();
        let script_chan = self.script_chan.clone();
        let f: ~fn(ImageResponseMsg) = |_| {
            script_chan.send(SendEventMsg(id.clone(), ReflowEvent))
        };
        f
    }
}

impl LayoutTask {
    /// Spawns a new layout task.
    pub fn create(id: PipelineId,
                  port: Port<Msg>,
                  constellation_chan: ConstellationChan,
                  script_chan: ScriptChan,
                  render_chan: RenderChan<AbstractNode<()>>,
                  img_cache_task: ImageCacheTask,
                  opts: Opts,
                  profiler_chan: ProfilerChan) {
        spawn_with!(task::task(), [port, constellation_chan, script_chan,
                                   render_chan, img_cache_task, profiler_chan], {
            let mut layout = LayoutTask::new(id,
                                             port,
                                             constellation_chan,
                                             script_chan,
                                             render_chan,
                                             img_cache_task,
                                             &opts,
                                             profiler_chan);
            layout.start();
        });
    }

    /// Creates a new `LayoutTask` structure.
    fn new(id: PipelineId,
           port: Port<Msg>,
           constellation_chan: ConstellationChan,
           script_chan: ScriptChan,
           render_chan: RenderChan<AbstractNode<()>>, 
           image_cache_task: ImageCacheTask,
           opts: &Opts,
           profiler_chan: ProfilerChan)
           -> LayoutTask {

        LayoutTask {
            id: id,
            port: port,
            constellation_chan: constellation_chan,
            script_chan: script_chan,
            render_chan: render_chan,
            image_cache_task: image_cache_task.clone(),
            local_image_cache: MutexArc::new(LocalImageCache(image_cache_task)),
            screen_size: None,

            display_list: None,

            stylist: RWArc::new(new_stylist()),
            profiler_chan: profiler_chan,
            opts: opts.clone()
        }
    }

    /// Starts listening on the port.
    fn start(&mut self) {
        while self.handle_request() {
            // Loop indefinitely.
        }
    }

    // Create a layout context for use in building display lists, hit testing, &c.
    fn build_layout_context(&self) -> LayoutContext {
        let image_cache = self.local_image_cache.clone();
        let font_ctx = ~FontContext::new(self.opts.render_backend, true,
                                            self.profiler_chan.clone());
        let screen_size = self.screen_size.unwrap();

        LayoutContext {
            image_cache: image_cache,
            font_ctx: font_ctx,
            screen_size: Rect(Point2D(Au(0), Au(0)), screen_size),
            constellation_chan: self.constellation_chan.clone(),
        }
    }

    /// Receives and dispatches messages from the port.
    fn handle_request(&mut self) -> bool {
        match self.port.recv() {
            AddStylesheetMsg(sheet) => self.handle_add_stylesheet(sheet),
            ReflowMsg(data) => {
                let data = Cell::new(data);

                do profile(time::LayoutPerformCategory, self.profiler_chan.clone()) {
                    self.handle_reflow(data.take());
                }
            }
            QueryMsg(query) => {
                let query = Cell::new(query);
                do profile(time::LayoutQueryCategory, self.profiler_chan.clone()) {
                    self.handle_query(query.take());
                }
            }
            ReapLayoutDataMsg(dead_layout_data) => {
                unsafe {
                    self.handle_reap_layout_data(dead_layout_data)
                }
            }
            PrepareToExitMsg(response_chan) => {
                debug!("layout: PrepareToExitMsg received");
                self.prepare_to_exit(response_chan);
                return false
            }
            ExitNowMsg => {
                debug!("layout: ExitNowMsg received");
                self.exit_now();
                return false
            }
        }

        true
    }

    /// Enters a quiescent state in which no new messages except for `ReapLayoutDataMsg` will be
    /// processed until an `ExitNowMsg` is received. A pong is immediately sent on the given
    /// response channel.
    fn prepare_to_exit(&mut self, response_chan: Chan<()>) {
        response_chan.send(());
        loop {
            match self.port.recv() {
                ReapLayoutDataMsg(dead_layout_data) => {
                    unsafe {
                        self.handle_reap_layout_data(dead_layout_data)
                    }
                }
                ExitNowMsg => {
                    self.exit_now();
                    break
                }
                _ => {
                    fail!("layout: message that wasn't `ExitNowMsg` received after \
                           `PrepareToExitMsg`")
                }
            }
        }
    }

    /// Shuts down the layout task now. If there are any DOM nodes left, layout will now (safely)
    /// crash.
    fn exit_now(&mut self) {
        let (response_port, response_chan) = stream();
        self.render_chan.send(render_task::ExitMsg(response_chan));
        response_port.recv()
    }

    fn handle_add_stylesheet(&mut self, sheet: Stylesheet) {
        let sheet = Cell::new(sheet);
        do self.stylist.write |stylist| {
            stylist.add_stylesheet(sheet.take(), AuthorOrigin)
        }
    }

    /// Builds the flow tree.
    ///
    /// This corresponds to the various `nsCSSFrameConstructor` methods in Gecko or
    /// `createRendererIfNeeded` in WebKit. Note, however that in WebKit `createRendererIfNeeded`
    /// is intertwined with selector matching, making it difficult to compare directly. It is
    /// marked `#[inline(never)]` to aid benchmarking in sampling profilers.
    #[inline(never)]
    fn construct_flow_tree(&self, layout_context: &mut LayoutContext, node: AbstractNode<LayoutView>)
                           -> ~Flow: {
        node.traverse_postorder_mut(&mut FlowConstructor::init(layout_context));

        let result = match *node.mutate_layout_data().ptr {
            Some(ref mut layout_data) => {
                util::replace(&mut layout_data.flow_construction_result, NoConstructionResult)
            }
            None => fail!("no layout data for root node"),
        };
        let mut flow = match result {
            FlowConstructionResult(flow) => flow,
            _ => fail!("Flow construction didn't result in a flow at the root of the tree!"),
        };
        flow.mark_as_root();
        flow
    }

    /// Performs layout constraint solving.
    ///
    /// This corresponds to `Reflow()` in Gecko and `layout()` in WebKit/Blink and should be
    /// benchmarked against those two. It is marked `#[inline(never)]` to aid profiling.
    #[inline(never)]
    fn solve_constraints(&mut self,
                         layout_root: &mut Flow,
                         layout_context: &mut LayoutContext) {
        let _ = layout_root.traverse_postorder(&mut BubbleWidthsTraversal(layout_context));

        // FIXME(kmc): We want to prune nodes without the Reflow restyle damage
        // bit, but FloatContext values can't be reused, so we need to
        // recompute them every time.
        // NOTE: this currently computes borders, so any pruning should separate that operation out.
        let _ = layout_root.traverse_preorder(&mut AssignWidthsTraversal(layout_context));

        // For now, this is an inorder traversal
        // FIXME: prune this traversal as well
        let _ = layout_root.traverse_postorder(&mut
            AssignHeightsAndStoreOverflowTraversal(layout_context));
    }

    /// The high-level routine that performs layout tasks.
    fn handle_reflow(&mut self, data: &Reflow) {
        // FIXME: Isolate this transmutation into a "bridge" module.
        let node: &AbstractNode<LayoutView> = unsafe {
            transmute(&data.document_root)
        };

        debug!("layout: received layout request for: {:s}", data.url.to_str());
        debug!("layout: damage is {:?}", data.damage);
        debug!("layout: parsed Node tree");
        debug!("{:?}", node.dump());

        // Reset the image cache.
        unsafe {
            self.local_image_cache.unsafe_access(
                |cache| cache.next_round(self.make_on_image_available_cb()));
        }

        // true => Do the reflow with full style damage, because content
        // changed or the window was resized.
        let mut all_style_damage = match data.damage.level {
            ContentChangedDocumentDamage => true,
            _ => false
        };

        let screen_size = Size2D(Au::from_px(data.window_size.width as int),
                                 Au::from_px(data.window_size.height as int));
        if self.screen_size != Some(screen_size) {
            all_style_damage = true;
        }
        self.screen_size = Some(screen_size);

        // Create a layout context for use throughout the following passes.
        let mut layout_ctx = self.build_layout_context();

        // Initialize layout data for each node.
        //
        // FIXME: This is inefficient. We don't need an entire traversal to do this!
        do profile(time::LayoutAuxInitCategory, self.profiler_chan.clone()) {
            node.initialize_style_for_subtree();
        }

        // Perform CSS selector matching if necessary.
        match data.damage.level {
            ReflowDocumentDamage => {}
            _ => {
                do profile(time::LayoutSelectorMatchCategory, self.profiler_chan.clone()) {
                    node.match_subtree(self.stylist.clone());
                    node.cascade_subtree(None);
                }
            }
        }

        // Construct the flow tree.
        let mut layout_root = profile(time::LayoutTreeBuilderCategory,
                                      self.profiler_chan.clone(),
                                      || self.construct_flow_tree(&mut layout_ctx, *node));

        // Propagate damage.
        layout_root.traverse_preorder(&mut PropagateDamageTraversal {
            all_style_damage: all_style_damage
        });
        layout_root.traverse_postorder(&mut ComputeDamageTraversal.clone());

        // Perform the primary layout passes over the flow tree to compute the locations of all
        // the boxes.
        do profile(time::LayoutMainCategory, self.profiler_chan.clone()) {
            self.solve_constraints(layout_root, &mut layout_ctx)
        }

        debug!("layout: constraint solving done:");
        debug!("{:?}", layout_root.dump());

        // Build the display list if necessary, and send it to the renderer.
        if data.goal == ReflowForDisplay {
            do profile(time::LayoutDispListBuildCategory, self.profiler_chan.clone()) {
                let root_size = flow::base(layout_root).position.size;
                let display_list= ~Cell::new(DisplayList::<AbstractNode<()>>::new());
                let dirty = flow::base(layout_root).position.clone();
                layout_root.build_display_list(
                    &DisplayListBuilder {
                        ctx: &layout_ctx,
                    },
                    &dirty,
                    display_list);

                let display_list = Arc::new(display_list.take());

                    let mut color = color::rgba(255.0, 255.0, 255.0, 255.0);

                    for child in node.traverse_preorder() {
                      if child.type_id() == ElementNodeTypeId(HTMLHtmlElementTypeId) || 
                         child.type_id() == ElementNodeTypeId(HTMLBodyElementTypeId) {
                             let element_bg_color = child.style().get().resolve_color(
                                 child.style().get().Background.background_color
                             ).to_gfx_color();
                             match element_bg_color {
                                 color::rgba(0., 0., 0., 0.) => {}
                                 _ => {
                                   color = element_bg_color;
                                   break;
                               }
                             }
                      }
                    }

                let render_layer = RenderLayer {
                    display_list: display_list.clone(),
                    size: Size2D(root_size.width.to_nearest_px() as uint,
                                 root_size.height.to_nearest_px() as uint),
                    color: color
                };

                self.display_list = Some(display_list.clone());

                self.render_chan.send(RenderMsg(render_layer));
            } // time(layout: display list building)
        }

        // Tell script that we're done.
        //
        // FIXME(pcwalton): This should probably be *one* channel, but we can't fix this without
        // either select or a filtered recv() that only looks for messages of a given type.
        data.script_join_chan.send(());
        data.script_chan.send(ReflowCompleteMsg(self.id, data.id));
    }

    /// Handles a query from the script task. This is the main routine that DOM functions like
    /// `getClientRects()` or `getBoundingClientRect()` ultimately invoke.
    fn handle_query(&self, query: LayoutQuery) {
        match query {
            ContentBoxQuery(node, reply_chan) => {
                // FIXME: Isolate this transmutation into a single "bridge" module.
                let node: AbstractNode<()> = unsafe {
                    transmute(node)
                };

                fn union_boxes_for_node<'a>(
                                        accumulator: &mut Option<Rect<Au>>,
                                        mut iter: DisplayItemIterator<'a,AbstractNode<()>>,
                                        node: AbstractNode<()>) {
                    for item in iter {
                        union_boxes_for_node(accumulator, item.children(), node);
                        if item.base().extra == node {
                            match *accumulator {
                                None => *accumulator = Some(item.base().bounds),
                                Some(ref mut acc) => *acc = acc.union(&item.base().bounds),
                            }
                        }
                    }
                }

                let mut rect = None;
                let display_list = self.display_list.as_ref().unwrap().get();
                union_boxes_for_node(&mut rect, display_list.iter(), node);
                reply_chan.send(ContentBoxResponse(rect.unwrap_or(Au::zero_rect())))
            }
            ContentBoxesQuery(node, reply_chan) => {
                // FIXME: Isolate this transmutation into a single "bridge" module.
                let node: AbstractNode<()> = unsafe {
                    transmute(node)
                };

                fn add_boxes_for_node<'a>(
                                      accumulator: &mut ~[Rect<Au>],
                                      mut iter: DisplayItemIterator<'a,AbstractNode<()>>,
                                      node: AbstractNode<()>) {
                    for item in iter {
                        add_boxes_for_node(accumulator, item.children(), node);
                        if item.base().extra == node {
                            accumulator.push(item.base().bounds)
                        }
                    }
                }

                let mut boxes = ~[];
                let display_list = self.display_list.as_ref().unwrap().get();
                add_boxes_for_node(&mut boxes, display_list.iter(), node);
                reply_chan.send(ContentBoxesResponse(boxes))
            }
            HitTestQuery(_, point, reply_chan) => {
                fn hit_test(x: Au, y: Au, list: &[DisplayItem<AbstractNode<()>>])
                            -> Option<HitTestResponse> {
                    for item in list.rev_iter() {
                        match *item {
                            ClipDisplayItemClass(ref cc) => {
                                let ret = hit_test(x, y, cc.child_list);
                                if !ret.is_none() {
                                    return ret;
                                }
                            }
                            _ => {}
                        }
                    }

                    for item in list.rev_iter() {
                        match *item {
                            ClipDisplayItemClass(_) => continue,
                            _ => {}
                        }
                        let bounds = item.bounds();
                        // TODO this check should really be performed by a method of DisplayItem
                        if x < bounds.origin.x + bounds.size.width &&
                            bounds.origin.x <= x &&
                            y < bounds.origin.y + bounds.size.height &&
                            bounds.origin.y <= y {
                            let node: AbstractNode<LayoutView> = unsafe {
                                transmute(item.base().extra)
                            };
                            let resp = Some(HitTestResponse(node));
                            return resp;
                        }
                    }

                    let ret: Option<HitTestResponse> = None;
                    ret
                }
                let response = {
                    match self.display_list {
                        Some(ref list) => {
                            let display_list = list.get();
                            let (x, y) = (Au::from_frac_px(point.x as f64),
                                          Au::from_frac_px(point.y as f64));
                            let resp = hit_test(x,y,display_list.list);
                            if resp.is_none() {
                                Err(())
                            } else {
                                Ok(resp.unwrap())
                            }
                        }
                        None => {
                            error!("Can't hit test: no display list");
                            Err(())
                        },
                    }
                };

                reply_chan.send(response)
            }
        }
    }

    // When images can't be loaded in time to display they trigger
    // this callback in some task somewhere. This will send a message
    // to the script task, and ultimately cause the image to be
    // re-requested. We probably don't need to go all the way back to
    // the script task for this.
    fn make_on_image_available_cb(&self) -> ~ImageResponder:Send {
        // This has a crazy signature because the image cache needs to
        // make multiple copies of the callback, and the dom event
        // channel is not a copyable type, so this is actually a
        // little factory to produce callbacks
        ~LayoutImageResponder {
            id: self.id.clone(),
            script_chan: self.script_chan.clone(),
        } as ~ImageResponder:Send
    }

    /// Handles a message to destroy layout data. Layout data must be destroyed on *this* task
    /// because it contains local managed pointers.
    unsafe fn handle_reap_layout_data(&self, layout_data: LayoutDataRef) {
        let ptr: &mut Option<~LayoutData> = cast::transmute(layout_data.borrow_unchecked());
        *ptr = None
    }
}

