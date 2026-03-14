async fn compute(x: i32) -> i32 {
    let result = x * 2;
    let doubled = result + 1;
    doubled
}

async fn spawned_task(n: i32) -> i32 {
    let val = n + 10;
    let final_val = val * 3;
    final_val
}

#[tokio::main]
async fn main() {
    // Case 1: Plain async fn awaited directly
    let a = compute(5).await;
    println!("compute(5) = {}", a);

    // Case 2: Async fn inside tokio::spawn
    let handle = tokio::spawn(async {
        let result = spawned_task(7).await;
        println!("spawned_task(7) = {}", result);
        result
    });

    let b = handle.await.unwrap();
    println!("total = {}", a + b);
}
