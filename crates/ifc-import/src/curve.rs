//! Sampling the curves a file states into polylines.
//!
//! A profile boundary and a sweep's directrix are both curves, and both end up
//! as points. The one approximation made here is stated once: a sampled arc
//! stays within [`Sampler::tolerance`] of the true arc, and every sample sits
//! exactly on it. A curve form this does not read returns `None` rather than
//! a chord across it, so a caller can count what it did not draw.

use crate::place::{
    Affine, IDENTITY, Vec3, add, axis_placement, cartesian_point, coordinates, cross, direction,
    dot, length, normalize, scale, subtract,
};
use crate::step::{Entity, Parsed, Value};
use crate::units::Units;

/// How many straight steps approximate a `sweep`-radian arc of `radius` to
/// within `tolerance`.
#[must_use]
pub fn arc_steps(radius: f64, sweep: f64, tolerance: f64) -> usize {
    if radius <= 0.0 || sweep.abs() <= f64::EPSILON {
        return 1;
    }
    // The sagitta of a step of angle `a` is `radius * (1 - cos(a / 2))`.
    let widest = if tolerance >= radius {
        std::f64::consts::PI
    } else {
        2.0 * (1.0 - tolerance / radius).acos()
    };
    if widest <= f64::EPSILON {
        return 256;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let steps = (sweep.abs() / widest).ceil() as usize;
    steps.clamp(1, 256)
}

/// Reads curves out of one file, in metres and in the coordinates of whatever
/// states them.
pub struct Sampler<'parsed> {
    pub parsed: &'parsed Parsed,
    pub units: Units,
    /// The furthest a chord may sit from the arc it replaces, in metres.
    pub tolerance: f64,
}

impl Sampler<'_> {
    /// The polyline a curve reduces to, or `None` for a curve form this does
    /// not read.
    ///
    /// The result is in metres, in the coordinate system the curve is stated
    /// in. A closed curve does not repeat its first point.
    #[must_use]
    pub fn curve(&self, entity: &Entity) -> Option<Vec<Vec3>> {
        let points = self.sample(entity, 0)?;
        (points.len() >= 2).then_some(points)
    }

    fn sample(&self, entity: &Entity, depth: u8) -> Option<Vec<Vec3>> {
        if depth > 8 {
            return None;
        }
        match entity.type_name.as_str() {
            "IFCPOLYLINE" => self.polyline(entity),
            "IFCINDEXEDPOLYCURVE" => self.indexed_poly_curve(entity),
            "IFCCOMPOSITECURVE" | "IFCCOMPOSITECURVEONSURFACE" => self.composite(entity, depth),
            "IFCTRIMMEDCURVE" => self.trimmed(entity, depth),
            // An untrimmed conic is the whole of it.
            "IFCCIRCLE" | "IFCELLIPSE" => {
                let conic = self.conic(entity)?;
                Some(conic.sample(0.0, std::f64::consts::TAU, self.tolerance))
            }
            _ => None,
        }
    }

    fn polyline(&self, entity: &Entity) -> Option<Vec<Vec3>> {
        let mut points = Vec::new();
        for value in entity.attribute(0)?.as_list()? {
            let at = self.parsed.follow(Some(value)).and_then(cartesian_point)?;
            points.push(scale(at, self.units.length));
        }
        // A polyline stated as closed repeats its first point; the loops this
        // feeds do not.
        if points.len() > 2 && close_enough(points[0], points[points.len() - 1]) {
            points.pop();
        }
        Some(points)
    }

    /// `IfcIndexedPolyCurve`: a point list, and either the order to read it in
    /// or nothing, which means read it straight through.
    fn indexed_poly_curve(&self, entity: &Entity) -> Option<Vec<Vec3>> {
        let list = self.parsed.follow(entity.attribute(0))?;
        let mut points = Vec::new();
        for value in list.attribute(0)?.as_list()? {
            points.push(scale(coordinates(value)?, self.units.length));
        }
        let Some(segments) = entity.attribute(1).and_then(Value::as_list) else {
            return Some(points);
        };
        let at = |index: &Value| -> Option<Vec3> {
            let index = usize::try_from(index.as_integer()?).ok()?;
            // The indices are one-based, as every index into an IFC list is.
            points.get(index.checked_sub(1)?).copied()
        };
        let mut out: Vec<Vec3> = Vec::new();
        let mut push = |point: Vec3| {
            if out.last().is_none_or(|last| !close_enough(*last, point)) {
                out.push(point);
            }
        };
        for segment in segments {
            let Value::Typed(kind, indices) = segment else {
                return None;
            };
            let indices = indices.as_list()?;
            match kind.as_str() {
                "IFCLINEINDEX" => {
                    for index in indices {
                        push(at(index)?);
                    }
                }
                // Three points: the start, one the arc passes through, and the
                // end.
                "IFCARCINDEX" if indices.len() == 3 => {
                    let through = three_point_arc(
                        at(&indices[0])?,
                        at(&indices[1])?,
                        at(&indices[2])?,
                        self.tolerance,
                    );
                    for point in through {
                        push(point);
                    }
                }
                _ => return None,
            }
        }
        if out.len() > 2 && close_enough(out[0], out[out.len() - 1]) {
            out.pop();
        }
        Some(out)
    }

    fn composite(&self, entity: &Entity, depth: u8) -> Option<Vec<Vec3>> {
        let mut out: Vec<Vec3> = Vec::new();
        for value in entity.attribute(0)?.as_list()? {
            let segment = self.parsed.follow(Some(value))?;
            let parent = self.parsed.follow(segment.attribute(2))?;
            let mut points = self.sample(parent, depth + 1)?;
            // `SameSense` false means the segment runs against its parent.
            if segment.attribute(1).and_then(Value::as_enumeration) == Some("F") {
                points.reverse();
            }
            for point in points {
                if out.last().is_none_or(|last| !close_enough(*last, point)) {
                    out.push(point);
                }
            }
        }
        if out.len() > 2 && close_enough(out[0], out[out.len() - 1]) {
            out.pop();
        }
        (!out.is_empty()).then_some(out)
    }

    fn trimmed(&self, entity: &Entity, depth: u8) -> Option<Vec<Vec3>> {
        let basis = self.parsed.follow(entity.attribute(0))?;
        // The sense flag says whether the curve runs with its parameter or
        // against it, which decides which way round the two trims it goes.
        let agrees = entity.attribute(3).and_then(Value::as_enumeration) != Some("F");
        match basis.type_name.as_str() {
            "IFCCIRCLE" | "IFCELLIPSE" => {
                let conic = self.conic(basis)?;
                let first = self.trim_angle(&conic, entity.attribute(1))?;
                let second = self.trim_angle(&conic, entity.attribute(2))?;
                let (from, to) = sweep_between(first, second, agrees);
                Some(conic.sample(from, to, self.tolerance))
            }
            "IFCLINE" => {
                let origin = self
                    .parsed
                    .follow(basis.attribute(0))
                    .and_then(cartesian_point)
                    .map(|at| scale(at, self.units.length))?;
                let vector = self.parsed.follow(basis.attribute(1))?;
                let heading = self
                    .parsed
                    .follow(vector.attribute(0))
                    .and_then(direction)?;
                let magnitude = vector
                    .attribute(1)
                    .and_then(Value::as_number)
                    .unwrap_or(1.0);
                let step = scale(heading, magnitude * self.units.length);
                let first = self.trim_length(origin, step, entity.attribute(1))?;
                let second = self.trim_length(origin, step, entity.attribute(2))?;
                let (from, to) = if agrees {
                    (first, second)
                } else {
                    (second, first)
                };
                Some(vec![
                    add(origin, scale(step, from)),
                    add(origin, scale(step, to)),
                ])
            }
            // Anything else is trimmed by re-sampling the whole basis, which
            // is only right when the trim is the whole of it. It is not, so
            // the curve is refused instead.
            _ => {
                let _ = depth;
                None
            }
        }
    }

    /// `IfcCircle` or `IfcEllipse`, as a frame and two semi-axes.
    fn conic(&self, entity: &Entity) -> Option<Conic> {
        let placement = self
            .parsed
            .follow(entity.attribute(0))
            .and_then(|position| axis_placement(self.parsed, position, &self.units))
            .unwrap_or(IDENTITY);
        let (first, second) = if entity.type_name == "IFCCIRCLE" {
            let radius = entity.attribute(1).and_then(Value::as_number)? * self.units.length;
            (radius, radius)
        } else {
            (
                entity.attribute(1).and_then(Value::as_number)? * self.units.length,
                entity.attribute(2).and_then(Value::as_number)? * self.units.length,
            )
        };
        (first > 0.0 && second > 0.0).then_some(Conic {
            placement,
            first,
            second,
        })
    }

    /// One end of a trim, as an angle on the conic.
    fn trim_angle(&self, conic: &Conic, trim: Option<&Value>) -> Option<f64> {
        let members = trim?.as_list()?;
        for member in members {
            if let Value::Typed(kind, inner) = member {
                if kind == "IFCPARAMETERVALUE" {
                    return Some(inner.as_number()? * self.units.angle);
                }
            }
        }
        // No parameter: the file located the end by a point on the curve.
        for member in members {
            if let Some(at) = self.parsed.follow(Some(member)).and_then(cartesian_point) {
                return Some(conic.angle_of(scale(at, self.units.length)));
            }
        }
        None
    }

    /// One end of a trim, as a multiple of `step` from `origin`.
    fn trim_length(&self, origin: Vec3, step: Vec3, trim: Option<&Value>) -> Option<f64> {
        let members = trim?.as_list()?;
        for member in members {
            if let Value::Typed(kind, inner) = member {
                if kind == "IFCPARAMETERVALUE" {
                    return inner.as_number();
                }
            }
        }
        let squared = dot(step, step);
        if squared <= 0.0 {
            return None;
        }
        for member in members {
            if let Some(at) = self.parsed.follow(Some(member)).and_then(cartesian_point) {
                let at = scale(at, self.units.length);
                return Some(dot(subtract(at, origin), step) / squared);
            }
        }
        None
    }
}

/// A circle or an ellipse in its own frame.
struct Conic {
    placement: Affine,
    first: f64,
    second: f64,
}

impl Conic {
    fn at(&self, angle: f64) -> Vec3 {
        self.placement
            .point([self.first * angle.cos(), self.second * angle.sin(), 0.0])
    }

    /// The angle at which the conic passes closest to `at`.
    fn angle_of(&self, at: Vec3) -> f64 {
        let local = subtract(at, self.placement.origin);
        dot(local, self.placement.basis[1]).atan2(dot(local, self.placement.basis[0]))
    }

    /// The arc from `from` to `to`, including both ends.
    fn sample(&self, from: f64, to: f64, tolerance: f64) -> Vec<Vec3> {
        let steps = arc_steps(self.first.max(self.second), to - from, tolerance);
        #[allow(clippy::cast_precision_loss)]
        let points = (0..=steps)
            .map(|step| self.at((to - from).mul_add(step as f64 / steps as f64, from)))
            .collect::<Vec<Vec3>>();
        points
    }
}

/// The interval a trimmed conic runs over, once the sense flag has decided
/// which way round it goes. A trim that states the same angle twice is a whole
/// turn, which is what a circle trimmed to itself means.
fn sweep_between(first: f64, second: f64, agrees: bool) -> (f64, f64) {
    let turn = std::f64::consts::TAU;
    if agrees {
        let mut end = second;
        while end <= first + f64::EPSILON {
            end += turn;
        }
        (first, end)
    } else {
        let mut end = second;
        while end >= first - f64::EPSILON {
            end -= turn;
        }
        (first, end)
    }
}

/// The arc through three points, as a polyline from the first to the third.
///
/// Three points that fall on a line describe no arc; they are joined straight,
/// which is the same curve.
#[must_use]
pub fn three_point_arc(start: Vec3, through: Vec3, end: Vec3, tolerance: f64) -> Vec<Vec3> {
    let straight = vec![start, through, end];
    // The circumcentre of the triangle the three points make, which is the
    // centre of the one circle through all three.
    let first = subtract(through, start);
    let second = subtract(end, start);
    let normal = cross(first, second);
    let squared = dot(normal, normal);
    if squared <= 1e-24 {
        return straight;
    }
    let center = add(
        start,
        scale(
            add(
                scale(cross(normal, first), dot(second, second)),
                scale(cross(second, normal), dot(first, first)),
            ),
            1.0 / (2.0 * squared),
        ),
    );
    let (Some(axis), Some(x)) = (normalize(normal), normalize(subtract(start, center))) else {
        return straight;
    };
    let radius = length(subtract(start, center));
    if !radius.is_finite() || radius <= 0.0 {
        return straight;
    }
    let y = cross(axis, x);
    // With `x` through the first point, the arc leaves at angle zero and the
    // frame's own handedness carries it through the middle point.
    let turn = std::f64::consts::TAU;
    let angle_of = |at: Vec3| {
        let local = subtract(at, center);
        let angle = dot(local, y).atan2(dot(local, x));
        if angle <= 0.0 { angle + turn } else { angle }
    };
    let middle = angle_of(through);
    let mut last = angle_of(end);
    if last <= middle {
        last += turn;
    }
    let steps = arc_steps(radius, last, tolerance);
    #[allow(clippy::cast_precision_loss)]
    let mut points: Vec<Vec3> = (0..=steps)
        .map(|step| {
            let angle = last * (step as f64 / steps as f64);
            add(
                center,
                add(
                    scale(x, radius * angle.cos()),
                    scale(y, radius * angle.sin()),
                ),
            )
        })
        .collect();
    // The ends are the points the file stated, exactly.
    points[0] = start;
    let at = points.len() - 1;
    points[at] = end;
    points
}

#[must_use]
pub fn close_enough(left: Vec3, right: Vec3) -> bool {
    length(subtract(left, right)) <= 1e-9
}

#[cfg(test)]
mod tests {
    use super::{Sampler, arc_steps, three_point_arc};
    use crate::step::parse;
    use crate::units::Units;

    fn file(data: &str) -> crate::step::Parsed {
        let text = format!("ISO-10303-21;\nDATA;\n{data}ENDSEC;\nEND-ISO-10303-21;\n");
        parse(text.as_bytes()).expect("a STEP file")
    }

    fn millimetre_sampler(parsed: &crate::step::Parsed) -> Sampler<'_> {
        Sampler {
            parsed,
            units: Units {
                length: 0.001,
                angle: std::f64::consts::PI / 180.0,
                stated_length: true,
            },
            tolerance: 0.004,
        }
    }

    #[test]
    fn a_finer_tolerance_asks_for_more_steps() {
        assert!(
            arc_steps(1.0, std::f64::consts::TAU, 0.001)
                > arc_steps(1.0, std::f64::consts::TAU, 0.05)
        );
        assert_eq!(arc_steps(0.0, 1.0, 0.004), 1);
        assert!(arc_steps(1.0, std::f64::consts::TAU, 1e-9) <= 256);
    }

    #[test]
    fn reads_a_polyline_in_the_files_length_unit_without_repeating_its_close() {
        let parsed = file(
            "#1=IFCCARTESIANPOINT((0.,0.));\n\
             #2=IFCCARTESIANPOINT((1000.,0.));\n\
             #3=IFCCARTESIANPOINT((1000.,500.));\n\
             #4=IFCPOLYLINE((#1,#2,#3,#1));\n",
        );
        let points = millimetre_sampler(&parsed)
            .curve(parsed.get(4).expect("#4"))
            .expect("a polyline");
        assert_eq!(points.len(), 3);
        assert!((points[1][0] - 1.0).abs() < 1e-12);
        assert!((points[2][1] - 0.5).abs() < 1e-12);
    }

    #[test]
    fn samples_a_trimmed_circle_over_the_angle_unit_the_file_declared() {
        let parsed = file(
            "#1=IFCCARTESIANPOINT((0.,0.,0.));\n\
             #2=IFCAXIS2PLACEMENT3D(#1,$,$);\n\
             #3=IFCCIRCLE(#2,1000.);\n\
             #4=IFCTRIMMEDCURVE(#3,(IFCPARAMETERVALUE(0.)),(IFCPARAMETERVALUE(90.)),.T.,.PARAMETER.);\n",
        );
        let points = millimetre_sampler(&parsed)
            .curve(parsed.get(4).expect("#4"))
            .expect("an arc");
        let first = points[0];
        let last = points[points.len() - 1];
        assert!((first[0] - 1.0).abs() < 1e-9 && first[1].abs() < 1e-9);
        assert!(last[0].abs() < 1e-9 && (last[1] - 1.0).abs() < 1e-9);
        // Every sample sits on the circle it came from.
        for point in &points {
            assert!((point[0].hypot(point[1]) - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn reads_an_indexed_poly_curve_with_an_arc_in_it() {
        let parsed = file(
            "#1=IFCCARTESIANPOINTLIST2D(((1000.,0.),(707.10678,707.10678),(0.,1000.),(0.,0.)));\n\
             #2=IFCINDEXEDPOLYCURVE(#1,(IFCARCINDEX((1,2,3)),IFCLINEINDEX((3,4,1))),.F.);\n",
        );
        let points = millimetre_sampler(&parsed)
            .curve(parsed.get(2).expect("#2"))
            .expect("a curve");
        // The quarter arc is sampled, then two straight sides close it.
        assert!(points.len() > 4);
        assert!((points[0][0] - 1.0).abs() < 1e-9);
        for point in &points {
            let radius = point[0].hypot(point[1]);
            assert!(radius <= 1.0 + 1e-9);
        }
    }

    #[test]
    fn refuses_a_curve_form_it_does_not_read() {
        let parsed =
            file("#1=IFCBSPLINECURVEWITHKNOTS(3,(),.UNSPECIFIED.,.F.,.F.,(),(),.UNSPECIFIED.);\n");
        assert!(
            millimetre_sampler(&parsed)
                .curve(parsed.get(1).expect("#1"))
                .is_none()
        );
    }

    #[test]
    fn joins_three_points_on_a_line_straight() {
        let points = three_point_arc([0.0; 3], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0], 0.004);
        assert_eq!(points.len(), 3);
        assert!((points[2][0] - 2.0).abs() < 1e-12);
    }
}
