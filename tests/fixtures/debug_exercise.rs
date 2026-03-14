#[derive(Debug)]
struct Point {
    x: i32,
    y: i32,
}

fn add(a: i32, b: i32) -> i32 {
    let sum = a + b;
    sum
}

fn classify(n: i32) -> &'static str {
    if n % 15 == 0 {
        "FizzBuzz"
    } else if n % 3 == 0 {
        "Fizz"
    } else if n % 5 == 0 {
        "Buzz"
    } else {
        "Other"
    }
}

fn process_point(p: &Point) -> i32 {
    p.x * p.x + p.y * p.y
}

fn iterate(count: i32) -> i32 {
    let mut total = 0;
    for i in 0..count {
        total += i;
    }
    total
}

fn main() {
    let a = 3;
    let b = 5;
    let c = add(a, b);
    let label = classify(c);
    let p = Point { x: a, y: b };
    let dist = process_point(&p);
    let sum = iterate(5);
    let result = c + dist + sum;
    println!("{} {} {}", label, result, sum);
}
