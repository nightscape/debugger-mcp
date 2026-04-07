#!/usr/bin/env python3
"""Test fixture with diverse debugging scenarios for integration tests."""


class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y
        self.label = f"({x}, {y})"

    def distance_from_origin(self):
        import math
        return math.sqrt(self.x ** 2 + self.y ** 2)


def factorial(n):
    """Recursive function for step_into/step_out testing."""
    if n <= 1:
        return 1
    return n * factorial(n - 1)


def will_crash(data):
    """Function that crashes for run_to_crash testing."""
    total = 0
    for item in data:
        total += item["value"]  # KeyError when "value" missing
    return total


def slow_loop():
    """Loop that takes time — for timeout/stuck testing."""
    total = 0
    for i in range(1, 51):
        total += i
    return total


def nested_data():
    """Returns nested data structures for variable inspection."""
    points = [Point(1, 2), Point(3, 4), Point(5, 6)]
    mapping = {
        "alpha": {"nested_key": [10, 20, 30]},
        "beta": {"nested_key": [40, 50, 60]},
    }
    matrix = [[1, 2, 3], [4, 5, 6], [7, 8, 9]]
    return points, mapping, matrix


def compute_with_locals(a, b):
    """Function with many local variables for variable inspection."""
    x = a + b
    y = a * b
    z = a - b
    name = "test_computation"
    flag = True
    items = [a, b, x, y, z]
    result = sum(items)
    return result


def main():
    print("=== Starting debug scenarios ===")

    val = compute_with_locals(3, 7)
    print(f"compute_with_locals(3, 7) = {val}")

    fact = factorial(5)
    print(f"factorial(5) = {fact}")

    total = slow_loop()
    print(f"slow_loop() = {total}")

    points, mapping, matrix = nested_data()
    print(f"points: {len(points)}, mapping keys: {list(mapping.keys())}")

    print("=== About to crash ===")
    data = [{"value": 1}, {"value": 2}, {"missing": 3}]
    try:
        will_crash(data)
    except KeyError as e:
        print(f"Caught expected crash: {e}")

    print("=== Done ===")


if __name__ == "__main__":
    main()
