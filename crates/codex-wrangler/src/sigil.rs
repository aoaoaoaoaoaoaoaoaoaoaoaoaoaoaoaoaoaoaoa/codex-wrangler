use egui::{Color32, Mesh, Painter, Rect, Shape, pos2, vec2};

use crate::contract::Harness;

const OPENAI: [u16; 12] = bits([
    "0000111110000",
    "0011100011100",
    "0110011100110",
    "1100110110011",
    "1001100011001",
    "1011000001101",
    "1011000001101",
    "1001100011001",
    "1100110110011",
    "0110011100110",
    "0011100011100",
    "0000111110000",
]);

const CLAUDE: [u16; 9] = bits([
    "0010000000100",
    "0001000001000",
    "0011111111100",
    "0110111110110",
    "1111111111111",
    "1011111111101",
    "1010000000101",
    "0001100011000",
    "0011000001100",
]);

const PRIME: [u16; 11] = bits([
    "0000000001111",
    "0111000011110",
    "1111100111100",
    "0111111111000",
    "0011111110000",
    "0001111100000",
    "0011111110000",
    "0111101111000",
    "1111000111100",
    "1110000011110",
    "0100000001100",
]);

const WIDTH: usize = 13;

pub fn paint(painter: &Painter, bounds: Rect, harness: Harness) {
    let (rows, row_count, color): (&[u16], f32, Color32) = match harness {
        Harness::Codex => (&OPENAI, 12.0, Color32::from_rgb(226, 231, 224)),
        Harness::ClaudeCode => (&CLAUDE, 9.0, Color32::from_rgb(222, 126, 91)),
        Harness::PrimeAgent => (&PRIME, 11.0, Color32::from_rgb(166, 145, 255)),
    };
    let cell = (bounds.height() / (row_count + 4.0)).min(1.5);
    let size = vec2(13.0 * cell, row_count * cell);
    let origin = bounds.center() - size * 0.5;
    let mut mesh = Mesh::default();
    let mut y = 0.0;
    for row in rows.iter().copied() {
        let mut x = 0.0;
        for column in 0..WIDTH {
            let mask = 1_u16 << (WIDTH - column - 1);
            if row & mask != 0 {
                let min = pos2(origin.x + x * cell, origin.y + y * cell);
                mesh.add_colored_rect(Rect::from_min_size(min, vec2(cell, cell)), color);
            }
            x += 1.0;
        }
        y += 1.0;
    }
    painter.add(Shape::mesh(mesh));
}

const fn bits<const N: usize>(rows: [&str; N]) -> [u16; N] {
    let mut parsed = [0_u16; N];
    let mut y = 0;
    while y < N {
        let bytes = rows[y].as_bytes();
        let mut x = 0;
        while x < bytes.len() {
            parsed[y] = (parsed[y] << 1) | if bytes[x] == b'1' { 1 } else { 0 };
            x += 1;
        }
        y += 1;
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sigil_is_bounded_distinct_and_nontrivial() {
        let sigils = [&OPENAI[..], &CLAUDE[..], &PRIME[..]];
        for rows in sigils {
            let pixels = rows.iter().map(|row| row.count_ones()).sum::<u32>();
            assert!((24..=90).contains(&pixels));
            assert!(rows.iter().all(|row| *row < (1 << WIDTH)));
        }
        assert_ne!(OPENAI.as_slice(), CLAUDE.as_slice());
        assert_ne!(CLAUDE.as_slice(), PRIME.as_slice());
    }
}
