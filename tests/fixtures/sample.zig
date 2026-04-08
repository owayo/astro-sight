const std = @import("std");
const math = @import("std").math;

pub fn add(a: u32, b: u32) u32 {
    return a + b;
}

fn helper() void {}

pub const Point = struct {
    x: f64,
    y: f64,

    pub fn init(x: f64, y: f64) Point {
        return .{ .x = x, .y = y };
    }
};

pub const Color = enum { red, green, blue };

const Direction = union(enum) {
    up: void,
    down: void,
};

test "add works" {
    const result = add(1, 2);
    try std.testing.expectEqual(@as(u32, 3), result);
}
