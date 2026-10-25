//! Reed-Solomon over GF(256), systematic, built on a Cauchy matrix.
//!
//! Parity row `i` of a group with `p` parity and `k` data packets is
//! `sum_j d_j / (i xor (p + j))`. The two sets `0..p` and `p..p+k` never meet, so no divisor is
//! zero, and every square piece of a Cauchy matrix is invertible: any `k` of the `k + p` packets
//! rebuild the group. `k + p` is at most 255.

const fn build_tables() -> ([u8; 512], [u8; 256]) {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11d;
        }
        i += 1;
    }
    let mut j = 255;
    while j < 512 {
        exp[j] = exp[j - 255];
        j += 1;
    }
    (exp, log)
}

static TABLES: ([u8; 512], [u8; 256]) = build_tables();

pub(crate) fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        TABLES.0[TABLES.1[a as usize] as usize + TABLES.1[b as usize] as usize]
    }
}

fn inv(a: u8) -> u8 {
    debug_assert!(a != 0);
    TABLES.0[255 - TABLES.1[a as usize] as usize]
}

/// `dst ^= c * src`.
fn mul_acc(dst: &mut [u8], src: &[u8], c: u8) {
    if c == 0 {
        return;
    }
    if c == 1 {
        for (d, s) in dst.iter_mut().zip(src) {
            *d ^= *s;
        }
        return;
    }
    let mut table = [0u8; 256];
    for (v, t) in table.iter_mut().enumerate() {
        *t = mul(c, v as u8);
    }
    for (d, s) in dst.iter_mut().zip(src) {
        *d ^= table[*s as usize];
    }
}

/// The coefficient of data packet `j` in parity packet `i` of a group with `p` parity packets.
fn coef(i: usize, j: usize, p: usize) -> u8 {
    inv((i as u8) ^ ((p + j) as u8))
}

/// Parity packets for `data`, all of one length. `data.len() + parity` must be at most 255.
pub(crate) fn encode(data: &[Vec<u8>], parity: usize) -> Vec<Vec<u8>> {
    debug_assert!(data.len() + parity <= 255);
    let len = data.first().map_or(0, Vec::len);
    (0..parity)
        .map(|i| {
            let mut out = vec![0u8; len];
            for (j, d) in data.iter().enumerate() {
                mul_acc(&mut out, d, coef(i, j, parity));
            }
            out
        })
        .collect()
}

/// Gauss-Jordan inverse of a square matrix over GF(256).
fn invert(a: &mut [Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    let n = a.len();
    let mut id: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            let mut row = vec![0u8; n];
            row[i] = 1;
            row
        })
        .collect();
    for col in 0..n {
        let pivot = (col..n).find(|&r| a[r][col] != 0)?;
        a.swap(col, pivot);
        id.swap(col, pivot);
        let ip = inv(a[col][col]);
        for c in 0..n {
            a[col][c] = mul(a[col][c], ip);
            id[col][c] = mul(id[col][c], ip);
        }
        for r in 0..n {
            if r == col {
                continue;
            }
            let f = a[r][col];
            if f == 0 {
                continue;
            }
            for c in 0..n {
                let x = mul(f, a[col][c]);
                a[r][c] ^= x;
                let y = mul(f, id[col][c]);
                id[r][c] ^= y;
            }
        }
    }
    Some(id)
}

/// Fill in the missing data packets of a group. `shards` holds `k` data slots then `p` parity
/// slots, every present one `len` bytes long. Returns `false` when too few packets are present.
pub(crate) fn reconstruct(k: usize, p: usize, shards: &mut [Option<Vec<u8>>], len: usize) -> bool {
    debug_assert_eq!(shards.len(), k + p);
    let missing: Vec<usize> = (0..k).filter(|&j| shards[j].is_none()).collect();
    let e = missing.len();
    if e == 0 {
        return true;
    }
    let rows: Vec<usize> = (0..p)
        .filter(|&i| shards[k + i].is_some())
        .take(e)
        .collect();
    if rows.len() < e {
        return false;
    }
    // What each chosen parity packet holds once the known data is taken out.
    let mut syndromes: Vec<Vec<u8>> = rows
        .iter()
        .map(|&i| shards[k + i].clone().unwrap_or_default())
        .collect();
    for (r, &i) in rows.iter().enumerate() {
        for (j, shard) in shards.iter().enumerate().take(k) {
            if let Some(d) = shard {
                mul_acc(&mut syndromes[r], d, coef(i, j, p));
            }
        }
    }
    let mut m: Vec<Vec<u8>> = rows
        .iter()
        .map(|&i| missing.iter().map(|&j| coef(i, j, p)).collect())
        .collect();
    let Some(minv) = invert(&mut m) else {
        return false;
    };
    for (mi, &j) in missing.iter().enumerate() {
        let mut out = vec![0u8; len];
        for (r, s) in syndromes.iter().enumerate() {
            mul_acc(&mut out, s, minv[mi][r]);
        }
        shards[j] = Some(out);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn field_laws() {
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1);
            assert_eq!(mul(a, 1), a);
            assert_eq!(mul(a, 0), 0);
        }
        assert_eq!(mul(2, 0x80), 0x1d);
    }

    #[test]
    fn any_k_of_n_rebuild_the_group() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        for &(k, p) in &[
            (1, 1),
            (1, 3),
            (2, 1),
            (5, 2),
            (10, 4),
            (40, 12),
            (127, 127),
            (200, 55),
        ] {
            let len = 37;
            let data: Vec<Vec<u8>> = (0..k)
                .map(|_| (0..len).map(|_| rng.next() as u8).collect())
                .collect();
            let parity = encode(&data, p);
            for _ in 0..8 {
                let mut shards: Vec<Option<Vec<u8>>> = data
                    .iter()
                    .cloned()
                    .chain(parity.iter().cloned())
                    .map(Some)
                    .collect();
                // lose exactly p packets anywhere
                let mut lost = 0;
                while lost < p {
                    let at = (rng.next() % (k + p) as u64) as usize;
                    if shards[at].take().is_some() {
                        lost += 1;
                    }
                }
                assert!(reconstruct(k, p, &mut shards, len), "k={k} p={p}");
                for j in 0..k {
                    assert_eq!(shards[j].as_ref(), Some(&data[j]), "k={k} p={p} j={j}");
                }
            }
        }
    }

    #[test]
    fn too_few_packets_are_refused() {
        let data = vec![vec![1u8, 2, 3], vec![4, 5, 6], vec![7, 8, 9]];
        let parity = encode(&data, 1);
        let mut shards = vec![None, None, Some(data[2].clone()), Some(parity[0].clone())];
        assert!(!reconstruct(3, 1, &mut shards, 3));
    }
}
