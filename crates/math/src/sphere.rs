use crate::DVec3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sphere {
    pub center: DVec3,
    pub radius: f64,
}

impl Sphere {
    pub fn union(self, other: Self) -> Self {
        let offset = other.center - self.center;
        let distance = offset.length();
        if distance + other.radius <= self.radius {
            return self;
        }
        if distance + self.radius <= other.radius {
            return other;
        }
        let radius = (distance + self.radius + other.radius) * 0.5;
        let center = self.center + offset * ((radius - self.radius) / distance);
        let radius = ((center - self.center).length() + self.radius).max((center - other.center).length() + other.radius);
        Self { center, radius: radius + radius * f64::EPSILON * 4.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_contains_both() {
        let spheres = [
            Sphere { center: DVec3::ZERO, radius: 1.0 },
            Sphere { center: DVec3::new(10.0, 0.0, 0.0), radius: 2.0 },
            Sphere { center: DVec3::new(0.5, 0.0, 0.0), radius: 0.25 },
            Sphere { center: DVec3::new(250_000.3, 99_999.7, 4_000.1), radius: 0.001 },
            Sphere { center: DVec3::new(250_016.9, 99_983.2, 4_011.0), radius: 3.5 },
        ];
        for a in spheres {
            for b in spheres {
                let union = a.union(b);
                for sphere in [a, b] {
                    assert!((union.center - sphere.center).length() + sphere.radius <= union.radius, "{a:?} ∪ {b:?} = {union:?}");
                }
            }
        }
        let outer = Sphere { center: DVec3::ZERO, radius: 5.0 };
        assert_eq!(outer.union(spheres[2]), outer);
        assert_eq!(spheres[2].union(outer), outer);
    }
}
