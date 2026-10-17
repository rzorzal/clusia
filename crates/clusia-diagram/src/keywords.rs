//! The keyword map in 3D: where each place sits in the cube of the three named axes, which
//! places bundle, which are kept under the point cap, the use-case clouds and the family lines.
//! Pure and deterministic: the same map always gives the same scene, bit for bit.

use std::collections::BTreeMap;

use clusia_core::keywords::{BUNDLE_OVER, KeywordMap, MAX_POINTS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointShape {
    Declared,
    Used,
    Bundle,
}

/// The use case of a place no region holds.
pub const ELSEWHERE: u16 = u16::MAX;
/// The radius of the ball a place's hashed offset falls in, in cube units.
pub const JITTER: f32 = 0.12;
pub const RADIUS_DECLARED: f32 = 0.044;
pub const RADIUS_USED: f32 = 0.028;
/// A bundle of `n` places has radius `RADIUS_USED + RADIUS_BUNDLE_GROWTH · √n`.
pub const RADIUS_BUNDLE_GROWTH: f32 = 0.0096;
pub const CLOUD_MIN: f32 = 0.15;
pub const CLOUD_MAX: f32 = 0.6;

#[derive(Clone, Debug, PartialEq)]
pub struct Point3 {
    /// Indexes into `KeywordMap.places`, ascending; more than one for a bundle.
    pub places: Vec<u32>,
    pub pos: [f32; 3],
    pub shape: PointShape,
    pub family: u16,
    pub use_case: u16,
    pub changed: bool,
    pub radius: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cloud {
    pub use_case: u16,
    pub centre: [f32; 3],
    pub radii: [f32; 3],
    /// Every place of the use case, also those past the point cap.
    pub places: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene3D {
    pub points: Vec<Point3>,
    /// `(declaration, point)` indexes into `points`.
    pub lines: Vec<(u32, u32)>,
    pub clouds: Vec<Cloud>,
    /// Places that are not drawn: past the point cap or past the search's caps.
    pub more: u32,
}

/// FNV-1a, 64 bit.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    fnv1a64_parts(&[bytes])
}

/// The hash of the parts joined, without joining them.
fn fnv1a64_parts(parts: &[&[u8]]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for part in parts {
        for &b in *part {
            h ^= u64::from(b);
            h = h.wrapping_mul(PRIME);
        }
    }
    h
}

/// A fixed offset in the ball of radius `JITTER`, hashed from the place's `path:line:word`, so
/// places of one region do not sit on one spot and the same place always sits at the same spot.
pub fn jitter(path: &str, line: u32, word: &str) -> [f32; 3] {
    // The line's digits, so the key hashes without an allocation per place.
    let mut digits = [0u8; 10];
    let mut at = digits.len();
    let mut rest = line;
    loop {
        at -= 1;
        digits[at] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    let mut h = fnv1a64_parts(&[path.as_bytes(), b":", &digits[at..], b":", word.as_bytes()]);
    for _ in 0..16 {
        let d = [unit(h), unit(h >> 21), unit(h >> 42)];
        if d.iter().map(|v| v * v).sum::<f32>() <= 1.0 {
            return d.map(|v| v * JITTER);
        }
        h = fnv1a64(&h.to_le_bytes());
    }
    [0.0; 3]
}

/// The low 21 bits of `bits` as a number in −1..1.
fn unit(bits: u64) -> f32 {
    const MASK: u64 = 0x1F_FFFF;
    (bits & MASK) as f32 / MASK as f32 * 2.0 - 1.0
}

/// A point before the cap: one place, or the bundled places of one word, file and use case.
struct Candidate {
    places: Vec<u32>,
    pos: [f32; 3],
    shape: PointShape,
    family: u16,
    use_case: u16,
    changed: bool,
    radius: f32,
}

/// Lays the map out in the cube. Places whose indexes do not fit the map's tables are left out.
pub fn scene(map: &KeywordMap) -> Scene3D {
    let overflow = map
        .overflow
        .iter()
        .fold(0, |a: u32, n| a.saturating_add(*n));
    let count = map.places.len();
    let usable: Vec<bool> = map
        .places
        .iter()
        .map(|p| {
            map.paths.get(p.path as usize).is_some() && map.words.get(usize::from(p.word)).is_some()
        })
        .collect();

    // Positions: placed places first, then each file's mean for the others.
    let mut pos = vec![[0.0f32; 3]; count];
    let mut jitters = vec![[0.0f32; 3]; count];
    let mut sums: Vec<Option<([f32; 3], u32)>> = vec![None; map.paths.len()];
    let mut placed = vec![false; count];
    for (i, place) in map.places.iter().enumerate() {
        if !usable[i] {
            continue;
        }
        let j = jitter(
            &map.paths[place.path as usize],
            place.line,
            &map.words[usize::from(place.word)].text,
        );
        jitters[i] = j;
        if let Some(region) = map.regions.get(usize::from(place.region)) {
            pos[i] = [0, 1, 2]
                .map(|a| (2.0 * region.at[a].clamp(0.0, 1.0) - 1.0 + j[a]).clamp(-1.0, 1.0));
            placed[i] = true;
            let entry = sums[place.path as usize].get_or_insert(([0.0; 3], 0));
            add(&mut entry.0, pos[i]);
            entry.1 += 1;
        }
    }
    for (i, place) in map.places.iter().enumerate() {
        if !usable[i] || placed[i] {
            continue;
        }
        pos[i] = match sums[place.path as usize] {
            Some((sum, n)) => {
                [0, 1, 2].map(|a| (sum[a] / n as f32 + jitters[i][a]).clamp(-1.0, 1.0))
            }
            None => [0, 1, 2].map(|a| jitters[i][a].clamp(-1.0, 1.0)),
        };
    }

    let use_case: Vec<u16> = map
        .places
        .iter()
        .map(|p| map.use_case_of(p).map_or(ELSEWHERE, |u| u as u16))
        .collect();
    let family: Vec<u16> = map.places.iter().map(|p| map.family_of(p) as u16).collect();

    let mut candidates = candidates(map, &usable, &pos, &use_case, &family);
    let mut more = overflow;
    if candidates.len() > MAX_POINTS {
        more = more.saturating_add(cap(&mut candidates));
    }
    candidates.sort_by_key(|c| c.places[0]);
    let points: Vec<Point3> = candidates
        .into_iter()
        .map(|c| Point3 {
            places: c.places,
            pos: c.pos,
            shape: c.shape,
            family: c.family,
            use_case: c.use_case,
            changed: c.changed,
            radius: c.radius,
        })
        .collect();

    let clouds = clouds(&points, &use_case, &usable);
    let lines = lines(&points);
    Scene3D {
        points,
        lines,
        clouds,
        more,
    }
}

/// The points before the cap: single places, and bundles of the many plain uses of one word in
/// one file, split by use case so a bundle never straddles two clouds.
fn candidates(
    map: &KeywordMap,
    usable: &[bool],
    pos: &[[f32; 3]],
    use_case: &[u16],
    family: &[u16],
) -> Vec<Candidate> {
    let mut order: Vec<u32> = (0..map.places.len() as u32)
        .filter(|&i| usable[i as usize])
        .collect();
    order.sort_by_key(|&i| {
        let p = &map.places[i as usize];
        (p.word, p.path, i)
    });
    let single = |i: u32| {
        let p = &map.places[i as usize];
        let declared = p.declared();
        Candidate {
            places: vec![i],
            pos: pos[i as usize],
            shape: if declared {
                PointShape::Declared
            } else {
                PointShape::Used
            },
            family: family[i as usize],
            use_case: use_case[i as usize],
            changed: p.changed(),
            radius: if declared {
                RADIUS_DECLARED
            } else {
                RADIUS_USED
            },
        }
    };
    let mut out = Vec::new();
    for group in order.chunk_by(|&a, &b| {
        let (a, b) = (&map.places[a as usize], &map.places[b as usize]);
        (a.word, a.path) == (b.word, b.path)
    }) {
        if group.len() <= BUNDLE_OVER {
            out.extend(group.iter().map(|&i| single(i)));
            continue;
        }
        let mut by_use_case: BTreeMap<u16, Vec<u32>> = BTreeMap::new();
        for &i in group {
            let p = &map.places[i as usize];
            if p.declared() || p.changed() {
                out.push(single(i));
            } else {
                by_use_case.entry(use_case[i as usize]).or_default().push(i);
            }
        }
        for (case, members) in by_use_case {
            if let [only] = members[..] {
                out.push(single(only));
                continue;
            }
            let n = members.len() as f32;
            let mut sum = [0.0f32; 3];
            for &i in &members {
                add(&mut sum, pos[i as usize]);
            }
            out.push(Candidate {
                pos: sum.map(|v| v / n),
                shape: PointShape::Bundle,
                family: family[members[0] as usize],
                use_case: case,
                changed: false,
                radius: RADIUS_USED + RADIUS_BUNDLE_GROWTH * n.sqrt(),
                places: members,
            });
        }
    }
    out
}

/// Keeps the first `MAX_POINTS` candidates by rank: changed, declared, bundles, then uses by
/// distance to the nearest change. Returns the places of the dropped ones.
fn cap(candidates: &mut Vec<Candidate>) -> u32 {
    let class = |c: &Candidate| match (c.changed, c.shape) {
        (true, _) => 0u8,
        (false, PointShape::Declared) => 1,
        (false, PointShape::Bundle) => 2,
        (false, PointShape::Used) => 3,
    };
    let strong = candidates.iter().filter(|c| class(c) < 3).count();
    let tree = if strong >= MAX_POINTS {
        KdTree::new(Vec::new())
    } else {
        KdTree::new(
            candidates
                .iter()
                .filter(|c| class(c) == 0)
                .map(|c| c.pos)
                .collect(),
        )
    };
    let mut ranks: Vec<(u8, f32, u32, usize)> = candidates
        .iter()
        .enumerate()
        .map(|(at, c)| {
            let class = class(c);
            let key = if class == 3 && strong < MAX_POINTS {
                tree.nearest(c.pos)
            } else {
                0.0
            };
            (class, key, c.places[0], at)
        })
        .collect();
    let by_rank = |a: &(u8, f32, u32, usize), b: &(u8, f32, u32, usize)| {
        a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.cmp(&b.2))
    };
    ranks.select_nth_unstable_by(MAX_POINTS, by_rank);
    let mut keep = vec![false; candidates.len()];
    for r in &ranks[..MAX_POINTS] {
        keep[r.3] = true;
    }
    let mut dropped = 0u32;
    let mut at = 0;
    candidates.retain(|c| {
        let kept = keep[at];
        at += 1;
        if !kept {
            dropped += c.places.len() as u32;
        }
        kept
    });
    dropped
}

/// One cloud per use case with a kept point: the mean of its points, spread by 1.5 standard
/// deviations per axis within bounds.
fn clouds(points: &[Point3], use_case: &[u16], usable: &[bool]) -> Vec<Cloud> {
    let mut by_case: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
    for (i, p) in points.iter().enumerate() {
        if p.use_case != ELSEWHERE {
            by_case.entry(p.use_case).or_default().push(i);
        }
    }
    let mut totals: BTreeMap<u16, u32> = BTreeMap::new();
    for (i, &case) in use_case.iter().enumerate() {
        if usable[i] && case != ELSEWHERE {
            *totals.entry(case).or_default() += 1;
        }
    }
    by_case
        .into_iter()
        .map(|(case, members)| {
            let n = members.len() as f32;
            let mut centre = [0.0f32; 3];
            for &i in &members {
                add(&mut centre, points[i].pos);
            }
            let centre = centre.map(|v| v / n);
            let mut variance = [0.0f32; 3];
            for &i in &members {
                let d = [0, 1, 2].map(|a| points[i].pos[a] - centre[a]);
                add(&mut variance, d.map(|v| v * v));
            }
            Cloud {
                use_case: case,
                centre,
                radii: variance.map(|v| (1.5 * (v / n).sqrt()).clamp(CLOUD_MIN, CLOUD_MAX)),
                places: totals.get(&case).copied().unwrap_or(0),
            }
        })
        .collect()
}

/// `(declaration, point)` for each kept point that is not a declaration, to the nearest kept
/// declaration of its family; a tie keeps the smaller index.
fn lines(points: &[Point3]) -> Vec<(u32, u32)> {
    let mut declarations: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
    for (i, p) in points.iter().enumerate() {
        if p.shape == PointShape::Declared {
            declarations.entry(p.family).or_default().push(i);
        }
    }
    let mut out = Vec::new();
    for (i, p) in points.iter().enumerate() {
        if p.shape == PointShape::Declared {
            continue;
        }
        let Some(candidates) = declarations.get(&p.family) else {
            continue;
        };
        let mut best: Option<(usize, f32)> = None;
        for &d in candidates {
            let dist = squared(p.pos, points[d].pos);
            if best.is_none_or(|(_, b)| dist < b) {
                best = Some((d, dist));
            }
        }
        if let Some((d, _)) = best {
            out.push((d as u32, i as u32));
        }
    }
    out
}

/// The changed points as a k-d tree, for nearest-point queries.
struct KdTree {
    points: Vec<[f32; 3]>,
}

impl KdTree {
    fn new(mut points: Vec<[f32; 3]>) -> Self {
        build(&mut points, 0);
        Self { points }
    }

    /// The squared distance from `p` to the nearest point; `+∞` with none.
    fn nearest(&self, p: [f32; 3]) -> f32 {
        let mut best = f32::INFINITY;
        search(&self.points, p, 0, &mut best);
        best
    }
}

/// Orders `points` so each slice's middle element splits it on `axis`, and its halves on the
/// next axis.
fn build(points: &mut [[f32; 3]], axis: usize) {
    if points.len() <= 1 {
        return;
    }
    let mid = points.len() / 2;
    points.select_nth_unstable_by(mid, |a, b| a[axis].total_cmp(&b[axis]));
    let (left, right) = points.split_at_mut(mid);
    build(left, (axis + 1) % 3);
    build(&mut right[1..], (axis + 1) % 3);
}

fn search(points: &[[f32; 3]], p: [f32; 3], axis: usize, best: &mut f32) {
    if points.is_empty() {
        return;
    }
    let mid = points.len() / 2;
    let here = points[mid];
    *best = best.min(squared(p, here));
    let diff = p[axis] - here[axis];
    let (near, far) = if diff < 0.0 {
        (&points[..mid], &points[mid + 1..])
    } else {
        (&points[mid + 1..], &points[..mid])
    };
    let next = (axis + 1) % 3;
    search(near, p, next, best);
    // Every point of the other half is at least `diff` away along this axis.
    if diff * diff < *best {
        search(far, p, next, best);
    }
}

fn add(acc: &mut [f32; 3], v: [f32; 3]) {
    for (a, v) in acc.iter_mut().zip(v) {
        *a += v;
    }
}

fn squared(a: [f32; 3], b: [f32; 3]) -> f32 {
    (0..3).map(|i| (a[i] - b[i]) * (a[i] - b[i])).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::keywords::{
        Axis, CHANGED, DECLARED, Family, KeywordPlan, MAX_POINTS, MapWord, NO_REGION, Place,
        Region, Seed, UseCase,
    };

    fn axis(name: &str) -> Axis {
        Axis {
            name: name.into(),
            low: "low".into(),
            high: "high".into(),
        }
    }

    /// A plan of `families` families and `use_cases` use cases `u0`, `u1`, ….
    fn plan(families: usize, use_cases: usize) -> KeywordPlan {
        KeywordPlan {
            title: "Where the words live".into(),
            caption: String::new(),
            axes: [axis("Layer"), axis("Time"), axis("Risk")],
            families: (0..families)
                .map(|f| Family {
                    id: format!("f{f}"),
                    label: format!("family {f}"),
                    seed: Seed {
                        word: format!("w{f}"),
                        path: "src/a.rs".into(),
                        line: 1,
                    },
                    words: vec![format!("w{f}")],
                    history: None,
                })
                .collect(),
            use_cases: (0..use_cases)
                .map(|u| UseCase {
                    id: format!("u{u}"),
                    name: format!("Use case {u}"),
                    note: None,
                    history: None,
                })
                .collect(),
        }
    }

    fn region(path: &str, use_case: usize, at: [f32; 3]) -> Region {
        Region {
            path: path.into(),
            start_line: None,
            end_line: None,
            use_case: format!("u{use_case}"),
            at,
        }
    }

    fn place(path: u32, line: u32, word: u16, region: u16, flags: u8) -> Place {
        Place {
            path,
            line,
            word,
            region,
            flags,
        }
    }

    /// A map over `paths`, words `w0`, `w1`, … of family `word / per_family`, with eight use cases.
    fn map(
        paths: &[&str],
        words: usize,
        per_family: usize,
        regions: Vec<Region>,
        mut places: Vec<Place>,
        overflow: Vec<u32>,
    ) -> KeywordMap {
        places.sort_by_key(|p| (p.path, p.line, p.word));
        KeywordMap {
            plan: plan(words.div_ceil(per_family).max(1), 8),
            regions,
            paths: paths.iter().map(|p| (*p).to_string()).collect(),
            words: (0..words)
                .map(|w| MapWord {
                    text: format!("w{w}"),
                    family: (w / per_family) as u16,
                })
                .collect(),
            places,
            overflow,
            search_cut: false,
        }
    }

    fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y) * (x - y))
            .sum::<f32>()
            .sqrt()
    }

    fn bits(scene: &Scene3D) -> Vec<u32> {
        scene
            .points
            .iter()
            .flat_map(|p| p.pos.iter().chain([&p.radius]).map(|v| v.to_bits()))
            .chain(
                scene
                    .clouds
                    .iter()
                    .flat_map(|c| c.centre.iter().chain(&c.radii).map(|v| v.to_bits())),
            )
            .collect()
    }

    #[test]
    fn fnv1a_is_the_standard_64_bit_hash() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn the_jitter_is_a_small_fixed_offset_per_place() {
        let a = jitter("src/a.rs", 10, "retry");
        assert_eq!(a, jitter("src/a.rs", 10, "retry"));
        assert_ne!(a, jitter("src/a.rs", 11, "retry"));
        assert_ne!(a, jitter("src/a.rs", 10, "backoff"));
        for line in 1..2_000 {
            let j = jitter("src/a.rs", line, "retry");
            assert!(dist(j, [0.0; 3]) <= JITTER + 1e-6, "{line}: {j:?}");
        }
    }

    #[test]
    fn a_place_sits_at_its_region_plus_its_jitter_inside_the_cube() {
        let m = map(
            &["src/a.rs"],
            1,
            1,
            vec![
                region("src/a.rs", 0, [0.5, 0.5, 0.5]),
                region("src/a.rs", 1, [1.0, 0.0, 1.0]),
            ],
            vec![place(0, 4, 0, 0, 0), place(0, 9, 0, 1, 0)],
            vec![0],
        );
        let s = scene(&m);
        assert_eq!(s.points.len(), 2);
        assert_eq!(s.points[0].pos, jitter("src/a.rs", 4, "w0"));
        assert_eq!((s.points[0].use_case, s.points[1].use_case), (0, 1));
        let corner = s.points[1].pos;
        assert!(
            corner.iter().all(|v| (-1.0..=1.0).contains(v)),
            "{corner:?}"
        );
        assert!(dist(corner, [1.0, -1.0, 1.0]) <= JITTER + 1e-6);
    }

    #[test]
    fn elsewhere_sits_with_its_files_placed_places_or_at_the_centre() {
        let m = map(
            &["src/a.rs", "src/b.rs"],
            1,
            1,
            vec![region("src/a.rs", 0, [1.0, 0.5, 0.5])],
            vec![
                place(0, 1, 0, 0, 0),
                place(0, 2, 0, NO_REGION, 0),
                place(1, 1, 0, NO_REGION, 0),
            ],
            vec![0],
        );
        let s = scene(&m);
        let placed = s.points[0].pos;
        let (beside, alone) = (&s.points[1], &s.points[2]);
        assert_eq!((beside.use_case, alone.use_case), (ELSEWHERE, ELSEWHERE));
        let expected = {
            let j = jitter("src/a.rs", 2, "w0");
            [0, 1, 2].map(|a| (placed[a] + j[a]).clamp(-1.0, 1.0))
        };
        assert_eq!(beside.pos, expected);
        assert_eq!(alone.pos, jitter("src/b.rs", 1, "w0"));
        assert!(
            s.clouds.iter().all(|c| c.use_case != ELSEWHERE),
            "Elsewhere has no cloud"
        );
    }

    #[test]
    fn shape_radius_and_ring_follow_the_flags() {
        let m = map(
            &["src/a.rs"],
            1,
            1,
            vec![region("src/a.rs", 0, [0.5, 0.5, 0.5])],
            vec![
                place(0, 1, 0, 0, DECLARED | CHANGED),
                place(0, 2, 0, 0, DECLARED),
                place(0, 3, 0, 0, CHANGED),
                place(0, 4, 0, 0, 0),
            ],
            vec![0],
        );
        let got: Vec<(PointShape, f32, bool)> = scene(&m)
            .points
            .iter()
            .map(|p| (p.shape, p.radius, p.changed))
            .collect();
        assert_eq!(
            got,
            [
                (PointShape::Declared, RADIUS_DECLARED, true),
                (PointShape::Declared, RADIUS_DECLARED, false),
                (PointShape::Used, RADIUS_USED, true),
                (PointShape::Used, RADIUS_USED, false),
            ]
        );
    }

    #[test]
    fn more_than_twelve_places_of_a_word_in_a_file_bundle_but_changes_and_declarations_stay() {
        let at = [0.5, 0.5, 0.5];
        let twelve = map(
            &["src/a.rs"],
            1,
            1,
            vec![region("src/a.rs", 0, at)],
            (1..=12).map(|line| place(0, line, 0, 0, 0)).collect(),
            vec![0],
        );
        assert!(
            scene(&twelve)
                .points
                .iter()
                .all(|p| p.shape == PointShape::Used),
            "twelve is not above twelve"
        );
        let mut places: Vec<Place> = (1..=11).map(|line| place(0, line, 0, 0, 0)).collect();
        places.push(place(0, 20, 0, 0, DECLARED));
        places.push(place(0, 21, 0, 0, CHANGED));
        let m = map(
            &["src/a.rs"],
            1,
            1,
            vec![region("src/a.rs", 0, at)],
            places,
            vec![0],
        );
        let s = scene(&m);
        let shapes: Vec<PointShape> = s.points.iter().map(|p| p.shape).collect();
        assert_eq!(
            shapes,
            [PointShape::Bundle, PointShape::Declared, PointShape::Used]
        );
        let bundle = &s.points[0];
        assert_eq!(bundle.places, (0..11).collect::<Vec<u32>>());
        assert_eq!(
            bundle.radius,
            RADIUS_USED + RADIUS_BUNDLE_GROWTH * 11f32.sqrt()
        );
        assert!(!bundle.changed);
        let mean = [0, 1, 2].map(|a| {
            (1..=11)
                .map(|line| jitter("src/a.rs", line, "w0")[a])
                .sum::<f32>()
                / 11.0
        });
        assert!(
            dist(bundle.pos, mean) < 1e-5,
            "{:?} vs {mean:?}",
            bundle.pos
        );
        // A bundle never spans two use cases.
        let mut split: Vec<Place> = (1..=7).map(|line| place(0, line, 0, 0, 0)).collect();
        split.extend((8..=13).map(|line| place(0, line, 0, 1, 0)));
        let m = map(
            &["src/a.rs"],
            1,
            1,
            vec![region("src/a.rs", 0, at), region("src/a.rs", 1, [0.0; 3])],
            split,
            vec![0],
        );
        let parts: Vec<(PointShape, u16, usize)> = scene(&m)
            .points
            .iter()
            .map(|p| (p.shape, p.use_case, p.places.len()))
            .collect();
        assert_eq!(
            parts,
            [(PointShape::Bundle, 0, 7), (PointShape::Bundle, 1, 6)]
        );
    }

    #[test]
    fn the_cap_keeps_changes_then_declarations_then_bundles_then_the_nearest_uses() {
        // Path 0: five changed places; path 1: ten declarations; path 2: thirteen uses of one
        // word (a bundle); paths 3..303 one use each near the changes, 303..603 one each far away.
        let mut paths: Vec<String> = vec!["c.rs".into(), "d.rs".into(), "b.rs".into()];
        paths.extend((0..600).map(|i| format!("u{i}.rs")));
        let names: Vec<&str> = paths.iter().map(String::as_str).collect();
        let near = [0.5, 0.5, 0.5];
        let far = [1.0, 1.0, 1.0];
        let regions = vec![region("c.rs", 0, near), region("d.rs", 1, far)];
        let mut places = Vec::new();
        places.extend((1..=5).map(|line| place(0, line, 0, 0, CHANGED)));
        places.extend((1..=10).map(|line| place(1, line, 1, 1, DECLARED)));
        places.extend((1..=13).map(|line| place(2, line, 2, 1, 0)));
        places.extend((3..303).map(|path| place(path, 1, 3, 0, 0)));
        places.extend((303..603).map(|path| place(path, 1, 3, 1, 0)));
        let m = map(&names, 4, 1, regions, places, vec![0, 0, 0, 7]);
        let s = scene(&m);
        assert_eq!(s.points.len(), MAX_POINTS);
        let count = |shape: PointShape| s.points.iter().filter(|p| p.shape == shape).count();
        assert_eq!(s.points.iter().filter(|p| p.changed).count(), 5);
        assert_eq!(count(PointShape::Declared), 10);
        assert_eq!(count(PointShape::Bundle), 1);
        let path_of = |p: &Point3| m.places[p.places[0] as usize].path;
        let kept_near = s
            .points
            .iter()
            .filter(|p| p.shape == PointShape::Used && !p.changed && (3..303).contains(&path_of(p)))
            .count();
        let kept_far = s
            .points
            .iter()
            .filter(|p| (303..603).contains(&path_of(p)))
            .count();
        assert_eq!((kept_near, kept_far), (300, MAX_POINTS - 16 - 300));
        assert_eq!(s.more, (600 - (MAX_POINTS as u32 - 16)) + 7);
        let firsts: Vec<u32> = s.points.iter().map(|p| p.places[0]).collect();
        assert!(firsts.windows(2).all(|w| w[0] < w[1]), "ordered by place");
    }

    #[test]
    fn clouds_wrap_their_points_within_bounds() {
        let m = map(
            &["a.rs", "b.rs", "c.rs"],
            1,
            1,
            vec![
                region("a.rs", 0, [0.0, 0.5, 0.5]),
                region("b.rs", 0, [1.0, 0.5, 0.5]),
                region("c.rs", 1, [0.5, 0.5, 0.5]),
            ],
            vec![
                place(0, 1, 0, 0, 0),
                place(1, 1, 0, 1, 0),
                place(2, 1, 0, 2, 0),
                place(2, 2, 0, NO_REGION, 0),
            ],
            vec![0],
        );
        let s = scene(&m);
        assert_eq!(
            s.clouds.len(),
            2,
            "one per use case with points; none for Elsewhere"
        );
        let (wide, one) = (&s.clouds[0], &s.clouds[1]);
        assert_eq!((wide.use_case, wide.places), (0, 2));
        assert_eq!((one.use_case, one.places), (1, 1));
        assert_eq!(wide.radii[0], CLOUD_MAX, "two points two units apart");
        assert!(wide.radii[1] >= CLOUD_MIN && wide.radii[1] <= CLOUD_MAX);
        assert_eq!(one.radii, [CLOUD_MIN; 3], "one point has no spread");
        assert_eq!(one.centre, s.points[2].pos);
        let mean = [0, 1, 2].map(|a| (s.points[0].pos[a] + s.points[1].pos[a]) / 2.0);
        assert!(dist(wide.centre, mean) < 1e-6);
    }

    #[test]
    fn lines_join_each_point_to_its_familys_nearest_declaration() {
        // Family 0 (words 0, 1): declarations at both ends of x, a use near each.
        // Family 1 (word 2): uses only, so no lines.
        let m = map(
            &["a.rs", "b.rs"],
            3,
            2,
            vec![
                region("a.rs", 0, [0.0, 0.5, 0.5]),
                region("b.rs", 0, [1.0, 0.5, 0.5]),
            ],
            vec![
                place(0, 1, 0, 0, DECLARED),
                place(0, 2, 1, 0, 0),
                place(0, 3, 2, 0, 0),
                place(1, 1, 0, 1, DECLARED),
                place(1, 2, 1, 1, 0),
                place(1, 3, 2, 1, 0),
            ],
            vec![0, 0, 0],
        );
        let s = scene(&m);
        assert_eq!(s.lines, [(0, 1), (3, 4)]);
    }

    #[test]
    fn an_empty_map_is_an_empty_scene_that_still_counts_the_overflow() {
        let m = map(&[], 2, 1, Vec::new(), Vec::new(), vec![4, 5]);
        assert_eq!(
            scene(&m),
            Scene3D {
                more: 9,
                ..Scene3D::default()
            }
        );
    }

    #[test]
    fn counters_of_an_edited_file_saturate_instead_of_overflowing() {
        let m = map(&[], 2, 1, Vec::new(), Vec::new(), vec![u32::MAX, 5]);
        assert_eq!(scene(&m).more, u32::MAX);
    }

    #[test]
    fn indexes_that_do_not_fit_the_map_are_skipped_not_panicked_on() {
        let mut m = map(
            &["src/a.rs"],
            1,
            1,
            vec![region("src/a.rs", 0, [0.5, 0.5, 0.5])],
            vec![
                place(0, 1, 0, 0, 0),
                place(9, 1, 0, 0, 0),
                place(0, 2, 7, 0, 0),
            ],
            vec![0],
        );
        m.places.sort_by_key(|p| (p.path, p.line, p.word));
        let s = scene(&m);
        assert_eq!(s.points.len(), 1);
    }

    /// A map at the caps: 20,000 places over 2,000 files and 96 words in twelve families, 300
    /// file-wide regions, one place in two hundred changed and one in four hundred declared, so
    /// most of the 400 points are uses ranked by their distance to a change.
    fn at_the_caps() -> KeywordMap {
        let paths: Vec<String> = (0..2_000).map(|i| format!("src/f{i}.rs")).collect();
        let names: Vec<&str> = paths.iter().map(String::as_str).collect();
        let regions = (0..300usize)
            .map(|i| {
                let t = i as f32 / 300.0;
                region(&paths[i], i % 8, [t, 1.0 - t, (t * 7.0).fract()])
            })
            .collect();
        let places = (0..20_000u32)
            .map(|k| {
                let path = k % 2_000;
                let flags = if k % 200 == 0 {
                    CHANGED
                } else if k % 400 == 1 {
                    DECLARED
                } else {
                    0
                };
                let region = if path < 300 { path as u16 } else { NO_REGION };
                place(path, k / 2_000 + 1, (k % 96) as u16, region, flags)
            })
            .collect();
        map(&names, 96, 8, regions, places, vec![3; 96])
    }

    #[test]
    fn the_same_map_gives_the_same_scene_bit_for_bit_within_the_budget() {
        let m = at_the_caps();
        let started = std::time::Instant::now();
        let first = scene(&m);
        let took = started.elapsed();
        let limit = if cfg!(debug_assertions) { 200 } else { 20 };
        assert!(took.as_millis() <= limit, "{took:?}");
        assert_eq!(first.points.len(), MAX_POINTS);
        let second = scene(&m.clone());
        assert_eq!(first, second);
        assert_eq!(bits(&first), bits(&second));
    }
}
