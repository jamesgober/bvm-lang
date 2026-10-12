//! The RC profile's cycle collector: synchronous trial deletion (Bacon and
//! Rajan, "Concurrent Cycle Collection in Reference Counted Systems",
//! ECOOP 2001, the synchronous algorithm; PHP's collector is the same
//! algorithm).
//!
//! A garbage cycle keeps every count in it above zero, so counting alone
//! never frees it. [`Heap::dec`] buffers every container-like object whose
//! count drops to a value above zero: a *possible root* (the only way a
//! cycle can become garbage is that one of its members loses an outside
//! holder). A collection then:
//!
//! 1. **marks gray** everything reachable from the possible roots,
//!    subtracting from each child's count the reference its gray parent
//!    holds: what is left of a count is what holds the object from outside
//!    the gray subgraph;
//! 2. **scans**: a gray object with something left is live, and it and
//!    everything it reaches are restored (**black**, counts added back);
//!    a gray object with nothing left is **white**;
//! 3. **collects** the white objects: garbage held only by other garbage.
//!    Their outgoing references were already subtracted, so they are freed
//!    without releasing their children again.
//!
//! **Shared storage.** Array and map storage shared copy-on-write between
//! containers counts its words once ([`crate::heap`]), so it is a node of its
//! own here: a container whose storage is shared points at the storage
//! node, whose count is the number of containers sharing it and whose
//! children are its words. Storage alone held by one container is part of
//! that container. Without storage nodes a cycle through shared storage
//! would never be found.
//!
//! **Finalizers.** White objects that need code before they go (a struct
//! with a destructor, a suspended coroutine to close) are not freed by the
//! collection that finds them: they, and everything they reach, are restored
//! (black), they are marked destructed or finalized, and handed back to be
//! queued. A later collection frees them if they are still garbage once
//! their code has run. White objects they do not reach are freed now, so a
//! program that keeps making destructor objects cannot keep other garbage
//! alive.
//!
//! Every phase is an explicit work list (no recursion, whatever the depth or
//! length of the cycles), and the work is counted so the VM can charge it
//! against the run's fuel.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bytecode_lang::CoroState;

use crate::dynv;
use crate::heap::{
    BUFFERED, COLOR, Heap, Object, ROOT_THRESHOLD, ROOT_THRESHOLD_MAX, STUCK, coro_children,
    map_words,
};
use crate::program::Program;

const BLACK: u8 = 0;
const GRAY: u8 = 1;
const WHITE: u8 = 2;

/// A node of the graph trial deletion walks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Node {
    /// The object in a slot.
    Obj(u32),
    /// Shared array or map storage, by address.
    Store(usize),
}

/// A shared storage node's state during one collection.
#[derive(Debug)]
struct Store {
    /// Containers sharing it, less the gray ones.
    count: usize,
    color: u8,
    /// A container holding it (to read its words).
    rep: u32,
}

/// What one collection did.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Swept {
    /// Nodes visited, over every phase (the work charged as fuel).
    pub(crate) visits: usize,
    /// Objects freed.
    pub(crate) freed: usize,
}

/// One collection's scratch state.
struct Pass<'a> {
    heap: &'a mut Heap,
    prog: &'a Program,
    stores: BTreeMap<usize, Store>,
    work: Vec<Node>,
    /// The work list of [`Pass::scan_black`], which runs inside a scan.
    black: Vec<Node>,
    kids: Vec<Node>,
    words: Vec<u64>,
    visits: usize,
}

impl Pass<'_> {
    fn color(&self, n: Node) -> u8 {
        match n {
            Node::Obj(i) => self
                .heap
                .slots
                .get(i as usize)
                .map_or(BLACK, |s| s.flags & COLOR),
            Node::Store(p) => self.stores.get(&p).map_or(BLACK, |s| s.color),
        }
    }

    fn set_color(&mut self, n: Node, c: u8) {
        match n {
            Node::Obj(i) => {
                if let Some(s) = self.heap.slots.get_mut(i as usize) {
                    s.flags = (s.flags & !COLOR) | c;
                }
            }
            Node::Store(p) => {
                if let Some(s) = self.stores.get_mut(&p) {
                    s.color = c;
                }
            }
        }
    }

    /// The count left after the gray parents' references were subtracted.
    fn count(&self, n: Node) -> usize {
        match n {
            Node::Obj(i) => self.heap.slots.get(i as usize).map_or(0, |s| s.rc as usize),
            Node::Store(p) => self.stores.get(&p).map_or(0, |s| s.count),
        }
    }

    fn sub(&mut self, n: Node) {
        match n {
            Node::Obj(i) => {
                if let Some(s) = self.heap.slots.get_mut(i as usize) {
                    s.rc = s.rc.saturating_sub(1);
                }
            }
            Node::Store(p) => {
                if let Some(s) = self.stores.get_mut(&p) {
                    s.count = s.count.saturating_sub(1);
                }
            }
        }
    }

    fn add(&mut self, n: Node) {
        match n {
            Node::Obj(i) => {
                if let Some(s) = self.heap.slots.get_mut(i as usize) {
                    s.rc = s.rc.saturating_add(1);
                }
            }
            Node::Store(p) => {
                if let Some(s) = self.stores.get_mut(&p) {
                    s.count += 1;
                }
            }
        }
    }

    /// The live, countable object a word names. A stuck object is not a
    /// node: its count is not exact, so it is treated as held from outside
    /// (never gray, never subtracted from).
    fn node_of(&self, w: u64) -> Option<Node> {
        if !dynv::is_ref(w) {
            return None;
        }
        let (index, tag) = dynv::ref_parts(w);
        let s = self.heap.slots.get(index as usize)?;
        (s.tag == tag && s.obj.is_some() && s.flags & STUCK == 0).then_some(Node::Obj(index))
    }

    /// Fills `self.kids` with the children of `n`, registering shared
    /// storage nodes on first sight.
    fn children(&mut self, n: Node) {
        self.kids.clear();
        self.words.clear();
        match n {
            Node::Obj(i) => {
                let Some(obj) = self.heap.slots.get(i as usize).and_then(|s| s.obj.as_ref()) else {
                    return;
                };
                let shared = match obj {
                    Object::Array(a) if a.elem.is_reference() => {
                        let p = Arc::as_ptr(&a.items) as usize;
                        (Arc::strong_count(&a.items) > 1)
                            .then_some((p, Arc::strong_count(&a.items)))
                    }
                    Object::Map(m) if m.key.is_reference() || m.value.is_reference() => {
                        let p = Arc::as_ptr(&m.store) as usize;
                        (Arc::strong_count(&m.store) > 1)
                            .then_some((p, Arc::strong_count(&m.store)))
                    }
                    _ => None,
                };
                match shared {
                    Some((p, holders)) => {
                        let _store = self.stores.entry(p).or_insert(Store {
                            count: holders,
                            color: BLACK,
                            rep: i,
                        });
                        self.kids.push(Node::Store(p));
                        return;
                    }
                    None => match obj {
                        Object::Struct(_) | Object::Func(_) => {
                            crate::heap::owned_children(obj, self.prog, &mut self.words);
                        }
                        Object::Coro(c) => coro_children(c, self.prog, &mut self.words),
                        other => crate::heap::plain_children(other, &mut self.words),
                    },
                }
            }
            Node::Store(p) => {
                let Some(rep) = self.stores.get(&p).map(|s| s.rep) else {
                    return;
                };
                match self
                    .heap
                    .slots
                    .get(rep as usize)
                    .and_then(|s| s.obj.as_ref())
                {
                    Some(Object::Array(a)) => self.words.extend(a.items.iter().copied()),
                    Some(Object::Map(m)) => map_words(m, &mut self.words),
                    _ => {}
                }
            }
        }
        for k in 0..self.words.len() {
            if let Some(node) = self.words.get(k).and_then(|&w| self.node_of(w)) {
                self.kids.push(node);
            }
        }
    }

    /// Phase 1 from one root.
    fn mark_gray(&mut self, root: Node) {
        if self.color(root) == GRAY {
            return;
        }
        self.set_color(root, GRAY);
        self.work.push(root);
        while let Some(n) = self.work.pop() {
            self.visits += 1;
            self.children(n);
            let kids = core::mem::take(&mut self.kids);
            for &c in &kids {
                self.sub(c);
                if self.color(c) != GRAY {
                    self.set_color(c, GRAY);
                    self.work.push(c);
                }
            }
            self.kids = kids;
        }
    }

    /// Phase 2 from one root; returns the nodes it colored white.
    fn scan(&mut self, root: Node, whites: &mut Vec<Node>) {
        self.work.push(root);
        while let Some(n) = self.work.pop() {
            if self.color(n) != GRAY {
                continue;
            }
            self.visits += 1;
            if self.count(n) > 0 {
                self.scan_black(n);
                continue;
            }
            self.set_color(n, WHITE);
            whites.push(n);
            self.children(n);
            self.work.extend_from_slice(&self.kids);
        }
    }

    /// Restores `n` and everything it reaches: black, counts added back.
    fn scan_black(&mut self, n: Node) {
        let mut work = core::mem::take(&mut self.black);
        work.clear();
        self.set_color(n, BLACK);
        work.push(n);
        while let Some(x) = work.pop() {
            self.visits += 1;
            self.children(x);
            let kids = core::mem::take(&mut self.kids);
            for &c in &kids {
                self.add(c);
                if self.color(c) != BLACK {
                    self.set_color(c, BLACK);
                    work.push(c);
                }
            }
            self.kids = kids;
        }
        self.black = work;
    }

    /// Whether a white object needs code before it goes.
    fn needs_finalizer(&self, i: u32) -> bool {
        match self.heap.slots.get(i as usize).and_then(|s| s.obj.as_ref()) {
            Some(Object::Struct(s)) => !s.destructed && self.heap.dtor_of(s.ty).is_some(),
            Some(Object::Coro(c)) => {
                !c.finalized && matches!(c.state, CoroState::Yielded | CoroState::Awaiting)
            }
            _ => false,
        }
    }
}

impl Heap {
    /// Collects the garbage cycles among the possible roots (all of them:
    /// the caller decides when; see [`Machine::collect`]). Objects needing a
    /// finalizer are kept and appended to `rescued` (heap order), already
    /// marked destructed or finalized; the caller queues them.
    ///
    /// [`Machine::collect`]: crate::machine::Machine::collect
    pub(crate) fn collect_cycles(&mut self, prog: &Program, rescued: &mut Vec<u64>) -> Swept {
        let roots = core::mem::take(&mut self.roots);
        let mut pass = Pass {
            heap: self,
            prog,
            stores: BTreeMap::new(),
            work: Vec::new(),
            black: Vec::new(),
            kids: Vec::new(),
            words: Vec::new(),
            visits: 0,
        };
        // Phase 1: every root still held by something.
        let mut live = Vec::with_capacity(roots.len());
        for &w in &roots {
            let (index, tag) = dynv::ref_parts(w);
            let Some(s) = pass.heap.slots.get_mut(index as usize) else {
                continue;
            };
            if s.tag != tag || s.obj.is_none() {
                continue;
            }
            s.flags &= !BUFFERED;
            if s.rc == 0 || s.flags & STUCK != 0 {
                // On the zero list (or not countable): not a cycle root.
                continue;
            }
            live.push(Node::Obj(index));
        }
        pass.visits += roots.len();
        for &n in &live {
            pass.mark_gray(n);
        }
        // Phase 2.
        let mut whites = Vec::new();
        for &n in &live {
            pass.scan(n, &mut whites);
        }
        // Finalizers: keep what they reach.
        let mut keep: Vec<u32> = whites
            .iter()
            .filter_map(|&n| match n {
                Node::Obj(i) if pass.needs_finalizer(i) => Some(i),
                _ => None,
            })
            .collect();
        keep.sort_unstable();
        for &i in &keep {
            if pass.color(Node::Obj(i)) == WHITE {
                pass.scan_black(Node::Obj(i));
            }
            if let Some(s) = pass.heap.slots.get_mut(i as usize) {
                match &mut s.obj {
                    Some(Object::Struct(o)) => o.destructed = true,
                    Some(Object::Coro(c)) => c.finalized = true,
                    _ => {}
                }
                rescued.push(dynv::from_ref(i, s.tag));
            }
        }
        // Phase 3: free what is still white. Shared storage needs nothing:
        // it goes when the last container holding it does.
        let mut freed = 0;
        for &n in &whites {
            if let Node::Obj(i) = n {
                if pass.color(n) == WHITE {
                    pass.visits += 1;
                    pass.heap.free_uncounted(i as usize);
                    freed += 1;
                }
            }
        }
        let visits = pass.visits;
        self.stats.collections += 1;
        self.stats.cycle_freed += freed as u64;
        Swept { visits, freed }
    }

    /// Adapts the root threshold after an automatic collection, as PHP's
    /// collector does: one that freed little raises it (the buffered roots
    /// were mostly live), one that freed much lowers it back.
    pub(crate) fn adapt_threshold(&mut self, freed: usize) {
        if freed < 100 {
            self.root_threshold = (self.root_threshold + ROOT_THRESHOLD).min(ROOT_THRESHOLD_MAX);
        } else if self.root_threshold > ROOT_THRESHOLD {
            self.root_threshold -= ROOT_THRESHOLD;
        }
    }
}
