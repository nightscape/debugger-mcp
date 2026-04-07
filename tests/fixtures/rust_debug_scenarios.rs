// Diverse debugging scenarios for Rust integration tests.
// Covers: local variables, structs, enums, recursion, loops, panics.

#[derive(Debug)]
struct Point {
    x: f64,
    y: f64,
}

impl Point {
    fn distance_from_origin(&self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}

#[derive(Debug)]
enum Shape {
    Circle(f64),
    Rectangle(f64, f64),
}

impl Shape {
    fn area(&self) -> f64 {
        match self {
            Shape::Circle(r) => std::f64::consts::PI * r * r,
            Shape::Rectangle(w, h) => w * h,
        }
    }
}

fn factorial(n: u64) -> u64 {
    if n <= 1 {
        return 1;
    }
    n * factorial(n - 1)
}

fn compute_with_locals(a: i32, b: i32) -> i32 {
    let x = a + b;
    let y = a * b;
    let z = a - b;
    let name = "test_computation";
    let flag = true;
    let items = vec![a, b, x, y, z];
    let result: i32 = items.iter().sum();
    println!("{} {} {}", name, flag, result);
    result
}

fn will_panic(data: &[Option<i32>]) -> i32 {
    let mut total = 0;
    for item in data {
        total += item.unwrap(); // panics on None
    }
    total
}

fn slow_loop(count: i32) -> i32 {
    let mut total = 0;
    for i in 0..count {
        total += i;
    }
    total
}

fn nested_data() -> (Vec<Point>, Vec<Vec<i32>>) {
    let points = vec![
        Point { x: 1.0, y: 2.0 },
        Point { x: 3.0, y: 4.0 },
        Point { x: 5.0, y: 6.0 },
    ];
    let matrix = vec![vec![1, 2, 3], vec![4, 5, 6], vec![7, 8, 9]];
    (points, matrix)
}

fn step_target_inner(val: i32) -> i32 {
    let doubled = val * 2;
    doubled + 1
}

fn step_target_outer(a: i32, b: i32) -> i32 {
    let first = step_target_inner(a);
    let second = step_target_inner(b);
    first + second
}

fn main() {
    println!("=== Starting Rust debug scenarios ===");

    let val = compute_with_locals(3, 7);
    println!("compute_with_locals(3, 7) = {}", val);

    let fact = factorial(5);
    println!("factorial(5) = {}", fact);

    let total = slow_loop(50);
    println!("slow_loop(50) = {}", total);

    let (points, matrix) = nested_data();
    println!("points: {}, matrix rows: {}", points.len(), matrix.len());

    let p = Point { x: 3.0, y: 4.0 };
    let dist = p.distance_from_origin();
    println!("distance = {}", dist);

    let shapes = vec![Shape::Circle(2.0), Shape::Rectangle(3.0, 4.0)];
    for s in &shapes {
        println!("{:?} area = {}", s, s.area());
    }

    let stepped = step_target_outer(10, 20);
    println!("step_target_outer(10, 20) = {}", stepped);

    let data = vec![Some(1), Some(2), Some(3)];
    let safe_total = will_panic(&data);
    println!("safe total = {}", safe_total);

    println!("=== Done ===");
}
