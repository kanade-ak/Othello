use anyhow::{Context, Result, bail};
use num_bigint::BigUint;
use num_traits::Zero;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::env;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

const DEFAULT_DB: &str = "othello_full_search.sqlite3";
const PASS_MOVE: i32 = 64;
const DIRECTIONS: [(i8, i8); 8] = [
    (-1, -1),
    (-1, 0),
    (-1, 1),
    (0, -1),
    (0, 1),
    (1, -1),
    (1, 0),
    (1, 1),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Side {
    Black,
    White,
}

impl Side {
    fn opposite(self) -> Self {
        match self {
            Self::Black => Self::White,
            Self::White => Self::Black,
        }
    }

    fn to_i64(self) -> i64 {
        match self {
            Self::Black => 0,
            Self::White => 1,
        }
    }

    fn from_i64(value: i64) -> Result<Self> {
        match value {
            0 => Ok(Self::Black),
            1 => Ok(Self::White),
            _ => bail!("invalid side value: {value}"),
        }
    }

    fn to_char(self) -> char {
        match self {
            Self::Black => 'b',
            Self::White => 'w',
        }
    }

    fn from_char(value: char) -> Result<Self> {
        match value {
            'b' | 'B' => Ok(Self::Black),
            'w' | 'W' => Ok(Self::White),
            _ => bail!("invalid side char: {value}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Board {
    black: u64,
    white: u64,
    side: Side,
}

impl Board {
    fn initial() -> Self {
        let mut black = 0u64;
        let mut white = 0u64;
        black |= bit_at(3, 4); // E4
        black |= bit_at(4, 3); // D5
        white |= bit_at(3, 3); // D4
        white |= bit_at(4, 4); // E5
        Self {
            black,
            white,
            side: Side::Black,
        }
    }

    fn from_key(key: &str) -> Result<Self> {
        if key.len() != 33 {
            bail!("invalid key length: {key}");
        }
        let black = u64::from_str_radix(&key[0..16], 16)
            .with_context(|| format!("invalid black mask in key: {key}"))?;
        let white = u64::from_str_radix(&key[16..32], 16)
            .with_context(|| format!("invalid white mask in key: {key}"))?;
        let side = Side::from_char(
            key.as_bytes()
                .get(32)
                .map(|b| *b as char)
                .context("missing side in key")?,
        )?;
        Ok(Self { black, white, side })
    }

    fn key(self) -> String {
        format!(
            "{:016x}{:016x}{}",
            self.black,
            self.white,
            self.side.to_char()
        )
    }

    fn player_bits(self) -> u64 {
        match self.side {
            Side::Black => self.black,
            Side::White => self.white,
        }
    }

    fn opponent_bits(self) -> u64 {
        match self.side {
            Side::Black => self.white,
            Side::White => self.black,
        }
    }

    fn occupied(self) -> u64 {
        self.black | self.white
    }

    fn empties(self) -> u8 {
        64 - self.occupied().count_ones() as u8
    }

    fn disc_counts(self) -> (u8, u8) {
        (self.black.count_ones() as u8, self.white.count_ones() as u8)
    }

    fn legal_moves(self) -> u64 {
        let mut moves = 0u64;
        let occupied = self.occupied();
        for square in 0..64 {
            let bit = 1u64 << square;
            if occupied & bit == 0 && self.flips_for(square) != 0 {
                moves |= bit;
            }
        }
        moves
    }

    fn flips_for(self, square: u8) -> u64 {
        let row = (square / 8) as i8;
        let col = (square % 8) as i8;
        let player = self.player_bits();
        let opponent = self.opponent_bits();
        let mut flips = 0u64;

        for (dr, dc) in DIRECTIONS {
            let mut r = row + dr;
            let mut c = col + dc;
            let mut line = 0u64;

            while in_board(r, c) {
                let b = bit_at(r as u8, c as u8);
                if opponent & b != 0 {
                    line |= b;
                } else if player & b != 0 {
                    flips |= line;
                    break;
                } else {
                    break;
                }
                r += dr;
                c += dc;
            }
        }

        flips
    }

    fn apply_move(self, square: u8) -> Self {
        let placed = 1u64 << square;
        let flips = self.flips_for(square);
        debug_assert!(flips != 0);

        match self.side {
            Side::Black => Self {
                black: self.black | placed | flips,
                white: self.white & !flips,
                side: Side::White,
            },
            Side::White => Self {
                black: self.black & !flips,
                white: self.white | placed | flips,
                side: Side::Black,
            },
        }
    }

    fn pass(self) -> Self {
        Self {
            black: self.black,
            white: self.white,
            side: self.side.opposite(),
        }
    }

    fn is_terminal(self) -> bool {
        if self.occupied() == u64::MAX {
            return true;
        }
        self.legal_moves() == 0 && self.pass().legal_moves() == 0
    }
}

fn bit_at(row: u8, col: u8) -> u64 {
    1u64 << (row * 8 + col)
}

fn in_board(row: i8, col: i8) -> bool {
    (0..8).contains(&row) && (0..8).contains(&col)
}

fn square_name(square: u8) -> String {
    let file = (b'A' + (square % 8)) as char;
    let rank = (b'1' + (square / 8)) as char;
    format!("{file}{rank}")
}

fn parse_big(value: &str) -> Result<BigUint> {
    BigUint::from_str(value).with_context(|| format!("invalid integer in database: {value}"))
}

fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open database {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(30))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS positions (
            key TEXT PRIMARY KEY,
            black_hex TEXT NOT NULL,
            white_hex TEXT NOT NULL,
            side INTEGER NOT NULL,
            empties INTEGER NOT NULL,
            legal_moves INTEGER,
            terminal INTEGER NOT NULL DEFAULT 0,
            expanded INTEGER NOT NULL DEFAULT 0,
            reach_count TEXT NOT NULL DEFAULT '0',
            propagated_count TEXT NOT NULL DEFAULT '0',
            value_black INTEGER
        );

        CREATE INDEX IF NOT EXISTS idx_positions_empties
            ON positions (empties);
        CREATE INDEX IF NOT EXISTS idx_positions_value_black
            ON positions (value_black);

        CREATE TABLE IF NOT EXISTS frontier (
            key TEXT PRIMARY KEY,
            FOREIGN KEY (key) REFERENCES positions(key) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS edges (
            parent_key TEXT NOT NULL,
            move INTEGER NOT NULL,
            child_key TEXT NOT NULL,
            PRIMARY KEY (parent_key, move, child_key),
            FOREIGN KEY (parent_key) REFERENCES positions(key) ON DELETE CASCADE,
            FOREIGN KEY (child_key) REFERENCES positions(key) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_edges_child
            ON edges (child_key);

        CREATE TABLE IF NOT EXISTS terminal_counts (
            black_discs INTEGER NOT NULL,
            white_discs INTEGER NOT NULL,
            result TEXT NOT NULL,
            paths TEXT NOT NULL,
            PRIMARY KEY (black_discs, white_discs)
        );
        "#,
    )?;

    conn.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', '1')",
        [],
    )?;
    Ok(())
}

fn reset_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        DROP TABLE IF EXISTS terminal_counts;
        DROP TABLE IF EXISTS edges;
        DROP TABLE IF EXISTS frontier;
        DROP TABLE IF EXISTS positions;
        DROP TABLE IF EXISTS meta;
        "#,
    )?;
    create_schema(conn)
}

fn ensure_initial(conn: &Connection) -> Result<()> {
    let initial = Board::initial();
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM positions", [], |row| row.get(0))?;
    if count == 0 {
        insert_position_conn(conn, initial)?;
        conn.execute(
            "UPDATE positions SET reach_count = '1' WHERE key = ?1",
            params![initial.key()],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO frontier(key) VALUES (?1)",
            params![initial.key()],
        )?;
    }
    Ok(())
}

fn insert_position_conn(conn: &Connection, board: Board) -> Result<()> {
    conn.execute(
        r#"
        INSERT OR IGNORE INTO positions
            (key, black_hex, white_hex, side, empties)
        VALUES
            (?1, ?2, ?3, ?4, ?5)
        "#,
        params![
            board.key(),
            format!("{:016x}", board.black),
            format!("{:016x}", board.white),
            board.side.to_i64(),
            board.empties() as i64
        ],
    )?;
    Ok(())
}

fn insert_position_tx(tx: &Transaction<'_>, board: Board) -> Result<()> {
    tx.execute(
        r#"
        INSERT OR IGNORE INTO positions
            (key, black_hex, white_hex, side, empties)
        VALUES
            (?1, ?2, ?3, ?4, ?5)
        "#,
        params![
            board.key(),
            format!("{:016x}", board.black),
            format!("{:016x}", board.white),
            board.side.to_i64(),
            board.empties() as i64
        ],
    )?;
    Ok(())
}

fn add_reach_count(tx: &Transaction<'_>, key: &str, delta: &BigUint) -> Result<()> {
    let old: String = tx.query_row(
        "SELECT reach_count FROM positions WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )?;
    let next = parse_big(&old)? + delta;
    tx.execute(
        "UPDATE positions SET reach_count = ?1 WHERE key = ?2",
        params![next.to_string(), key],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO frontier(key) VALUES (?1)",
        params![key],
    )?;
    Ok(())
}

fn add_terminal_paths(
    tx: &Transaction<'_>,
    black_discs: u8,
    white_discs: u8,
    delta: &BigUint,
) -> Result<()> {
    let result = if black_discs > white_discs {
        "black"
    } else if white_discs > black_discs {
        "white"
    } else {
        "draw"
    };
    let old: Option<String> = tx
        .query_row(
            "SELECT paths FROM terminal_counts WHERE black_discs = ?1 AND white_discs = ?2",
            params![black_discs as i64, white_discs as i64],
            |row| row.get(0),
        )
        .optional()?;
    let next = match old {
        Some(value) => parse_big(&value)? + delta,
        None => delta.clone(),
    };
    tx.execute(
        r#"
        INSERT INTO terminal_counts(black_discs, white_discs, result, paths)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(black_discs, white_discs) DO UPDATE
            SET paths = excluded.paths
        "#,
        params![
            black_discs as i64,
            white_discs as i64,
            result,
            next.to_string()
        ],
    )?;
    Ok(())
}

fn init_command(db_path: &Path, reset: bool) -> Result<()> {
    let conn = open_db(db_path)?;
    if reset {
        reset_schema(&conn)?;
    } else {
        create_schema(&conn)?;
    }
    ensure_initial(&conn)?;
    println!("initialized: {}", db_path.display());
    Ok(())
}

fn run_command(
    db_path: &Path,
    batch: usize,
    max_positions: Option<u64>,
    max_seconds: Option<u64>,
) -> Result<()> {
    let mut conn = open_db(db_path)?;
    create_schema(&conn)?;
    ensure_initial(&conn)?;

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc::set_handler(move || {
            stop.store(true, Ordering::SeqCst);
        })
        .context("failed to install Ctrl+C handler")?;
    }

    let started = Instant::now();
    let mut processed = 0u64;
    let mut last_report = Instant::now();

    loop {
        if stop.load(Ordering::SeqCst) {
            println!("stop requested; exiting after last committed batch");
            break;
        }
        if let Some(limit) = max_positions {
            if processed >= limit {
                break;
            }
        }
        if let Some(seconds) = max_seconds {
            if started.elapsed() >= Duration::from_secs(seconds) {
                break;
            }
        }

        let remaining = max_positions
            .map(|limit| limit.saturating_sub(processed) as usize)
            .unwrap_or(batch);
        let take = batch.min(remaining.max(1));
        let keys = fetch_frontier_keys(&conn, take)?;
        if keys.is_empty() {
            println!("frontier is empty; graph expansion is complete for this database");
            break;
        }

        let tx = conn.transaction()?;
        for key in keys {
            if let Some(limit) = max_positions {
                if processed >= limit {
                    break;
                }
            }
            let did_work = process_position(&tx, &key)?;
            if did_work {
                processed += 1;
            }
        }
        tx.commit()?;

        if last_report.elapsed() >= Duration::from_secs(5) {
            print_short_status(&conn, processed, started.elapsed())?;
            last_report = Instant::now();
        }
    }

    print_short_status(&conn, processed, started.elapsed())?;
    Ok(())
}

fn fetch_frontier_keys(conn: &Connection, limit: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT key
        FROM frontier
        ORDER BY key
        LIMIT ?1
        "#,
    )?;
    let rows = stmt.query_map(params![limit as i64], |row| row.get::<_, String>(0))?;
    let mut keys = Vec::new();
    for row in rows {
        keys.push(row?);
    }
    Ok(keys)
}

fn process_position(tx: &Transaction<'_>, key: &str) -> Result<bool> {
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT reach_count, propagated_count FROM positions WHERE key = ?1",
            params![key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let Some((reach_text, propagated_text)) = row else {
        tx.execute("DELETE FROM frontier WHERE key = ?1", params![key])?;
        return Ok(false);
    };

    let reach = parse_big(&reach_text)?;
    let propagated = parse_big(&propagated_text)?;
    if reach == propagated {
        tx.execute("DELETE FROM frontier WHERE key = ?1", params![key])?;
        return Ok(false);
    }
    if reach < propagated {
        bail!("database invariant broken: propagated_count exceeds reach_count for {key}");
    }

    let delta = &reach - &propagated;
    tx.execute(
        "UPDATE positions SET propagated_count = ?1 WHERE key = ?2",
        params![reach.to_string(), key],
    )?;

    let board = Board::from_key(key)?;
    let moves = board.legal_moves();
    let opponent_moves = board.pass().legal_moves();

    if moves == 0 && opponent_moves == 0 {
        let (black_discs, white_discs) = board.disc_counts();
        let value_black = black_discs as i64 - white_discs as i64;
        tx.execute(
            r#"
            UPDATE positions
            SET terminal = 1,
                expanded = 1,
                legal_moves = 0,
                value_black = ?1
            WHERE key = ?2
            "#,
            params![value_black, key],
        )?;
        add_terminal_paths(tx, black_discs, white_discs, &delta)?;
    } else if moves == 0 {
        let child = board.pass();
        insert_position_tx(tx, child)?;
        insert_edge(tx, key, PASS_MOVE, &child.key())?;
        add_reach_count(tx, &child.key(), &delta)?;
        tx.execute(
            r#"
            UPDATE positions
            SET terminal = 0,
                expanded = 1,
                legal_moves = 0
            WHERE key = ?1
            "#,
            params![key],
        )?;
    } else {
        let legal_count = moves.count_ones() as i64;
        for square in bits(moves) {
            let child = board.apply_move(square);
            insert_position_tx(tx, child)?;
            insert_edge(tx, key, square as i32, &child.key())?;
            add_reach_count(tx, &child.key(), &delta)?;
        }
        tx.execute(
            r#"
            UPDATE positions
            SET terminal = 0,
                expanded = 1,
                legal_moves = ?1
            WHERE key = ?2
            "#,
            params![legal_count, key],
        )?;
    }

    tx.execute("DELETE FROM frontier WHERE key = ?1", params![key])?;
    Ok(true)
}

fn insert_edge(tx: &Transaction<'_>, parent: &str, mv: i32, child: &str) -> Result<()> {
    tx.execute(
        r#"
        INSERT OR IGNORE INTO edges(parent_key, move, child_key)
        VALUES (?1, ?2, ?3)
        "#,
        params![parent, mv, child],
    )?;
    Ok(())
}

fn bits(mut mask: u64) -> impl Iterator<Item = u8> {
    std::iter::from_fn(move || {
        if mask == 0 {
            None
        } else {
            let square = mask.trailing_zeros() as u8;
            mask &= mask - 1;
            Some(square)
        }
    })
}

fn status_command(db_path: &Path) -> Result<()> {
    let conn = open_db(db_path)?;
    create_schema(&conn)?;

    let positions: i64 = scalar_i64(&conn, "SELECT COUNT(*) FROM positions")?;
    let frontier: i64 = scalar_i64(&conn, "SELECT COUNT(*) FROM frontier")?;
    let edges: i64 = scalar_i64(&conn, "SELECT COUNT(*) FROM edges")?;
    let terminal_positions: i64 =
        scalar_i64(&conn, "SELECT COUNT(*) FROM positions WHERE terminal = 1")?;
    let solved: i64 = scalar_i64(
        &conn,
        "SELECT COUNT(*) FROM positions WHERE value_black IS NOT NULL",
    )?;
    let expanded: i64 = scalar_i64(&conn, "SELECT COUNT(*) FROM positions WHERE expanded = 1")?;
    let min_empties: Option<i64> = conn
        .query_row("SELECT MIN(empties) FROM positions", [], |row| row.get(0))
        .optional()?
        .flatten();
    let max_empties: Option<i64> = conn
        .query_row("SELECT MAX(empties) FROM positions", [], |row| row.get(0))
        .optional()?
        .flatten();
    let terminal_paths = terminal_path_total(&conn)?;

    println!("database: {}", db_path.display());
    println!("positions: {positions}");
    println!("expanded positions: {expanded}");
    println!("frontier: {frontier}");
    println!("edges: {edges}");
    println!("terminal positions: {terminal_positions}");
    println!("solved/value_black positions: {solved}");
    println!(
        "empties range: {}..{}",
        min_empties
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string()),
        max_empties
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!("terminal path total: {terminal_paths}");

    let mut stmt = conn.prepare(
        r#"
        SELECT black_discs, white_discs, result, paths
        FROM terminal_counts
        ORDER BY black_discs DESC, white_discs DESC
        LIMIT 10
        "#,
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    println!("terminal score buckets (top 10 by score order):");
    for row in rows {
        let (black, white, result, paths) = row?;
        println!("  {black}-{white} {result}: {paths}");
    }
    Ok(())
}

fn scalar_i64(conn: &Connection, sql: &str) -> Result<i64> {
    conn.query_row(sql, [], |row| row.get(0))
        .map_err(Into::into)
}

fn terminal_path_total(conn: &Connection) -> Result<BigUint> {
    let mut stmt = conn.prepare("SELECT paths FROM terminal_counts")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut total = BigUint::zero();
    for row in rows {
        total += parse_big(&row?)?;
    }
    Ok(total)
}

fn print_short_status(conn: &Connection, processed: u64, elapsed: Duration) -> Result<()> {
    let positions: i64 = scalar_i64(conn, "SELECT COUNT(*) FROM positions")?;
    let frontier: i64 = scalar_i64(conn, "SELECT COUNT(*) FROM frontier")?;
    let edges: i64 = scalar_i64(conn, "SELECT COUNT(*) FROM edges")?;
    let rate = if elapsed.as_secs_f64() > 0.0 {
        processed as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    println!(
        "processed this run: {processed}, rate: {rate:.1}/s, positions: {positions}, edges: {edges}, frontier: {frontier}"
    );
    Ok(())
}

fn perft_command(depth: u8, divide: bool) -> Result<()> {
    let board = Board::initial();
    if divide {
        let moves = board.legal_moves();
        let mut total = BigUint::zero();
        for square in bits(moves) {
            let child = board.apply_move(square);
            let count = perft(child, depth.saturating_sub(1));
            total += &count;
            println!("{}: {count}", square_name(square));
        }
        println!("total: {total}");
    } else {
        println!("{}", perft(board, depth));
    }
    Ok(())
}

fn perft(board: Board, depth: u8) -> BigUint {
    if depth == 0 || board.is_terminal() {
        return BigUint::from(1u8);
    }

    let moves = board.legal_moves();
    if moves == 0 {
        return perft(board.pass(), depth - 1);
    }

    let mut total = BigUint::zero();
    for square in bits(moves) {
        total += perft(board.apply_move(square), depth - 1);
    }
    total
}

fn retrograde_command(db_path: &Path, batch: usize, max_passes: Option<u64>) -> Result<()> {
    let mut conn = open_db(db_path)?;
    create_schema(&conn)?;

    let mut pass = 0u64;
    loop {
        if let Some(limit) = max_passes {
            if pass >= limit {
                break;
            }
        }
        pass += 1;
        let candidates = fetch_unsolved_candidates(&conn, batch)?;
        if candidates.is_empty() {
            println!("no unsolved candidates found");
            break;
        }

        let tx = conn.transaction()?;
        let mut solved_this_pass = 0u64;
        for (key, side) in candidates {
            if let Some(value) = retrograde_value(&tx, &key, side)? {
                tx.execute(
                    "UPDATE positions SET value_black = ?1 WHERE key = ?2",
                    params![value, key],
                )?;
                solved_this_pass += 1;
            }
        }
        tx.commit()?;

        println!("retrograde pass {pass}: solved {solved_this_pass}");
        if solved_this_pass == 0 {
            break;
        }
    }
    Ok(())
}

fn fetch_unsolved_candidates(conn: &Connection, limit: usize) -> Result<Vec<(String, Side)>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT key, side
        FROM positions
        WHERE value_black IS NULL
        ORDER BY empties ASC, key ASC
        LIMIT ?1
        "#,
    )?;
    let rows = stmt.query_map(params![limit as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut result = Vec::new();
    for row in rows {
        let (key, side_value) = row?;
        result.push((key, Side::from_i64(side_value)?));
    }
    Ok(result)
}

fn retrograde_value(tx: &Transaction<'_>, key: &str, side: Side) -> Result<Option<i64>> {
    let mut stmt = tx.prepare(
        r#"
        SELECT p.value_black
        FROM edges e
        JOIN positions p ON p.key = e.child_key
        WHERE e.parent_key = ?1
        "#,
    )?;
    let values = stmt.query_map(params![key], |row| row.get::<_, Option<i64>>(0))?;
    let mut saw_child = false;
    let mut best: Option<i64> = None;
    for value in values {
        saw_child = true;
        let Some(value) = value? else {
            return Ok(None);
        };
        best = Some(match (best, side) {
            (None, _) => value,
            (Some(current), Side::Black) => current.max(value),
            (Some(current), Side::White) => current.min(value),
        });
    }

    if saw_child { Ok(best) } else { Ok(None) }
}

fn export_positions_csv(db_path: &Path, output_path: &Path) -> Result<()> {
    let conn = open_db(db_path)?;
    create_schema(&conn)?;
    let mut writer = std::fs::File::create(output_path)
        .with_context(|| format!("failed to create {}", output_path.display()))?;
    use std::io::Write;
    writeln!(
        writer,
        "key,black_hex,white_hex,side,empties,legal_moves,terminal,reach_count,value_black"
    )?;

    let mut stmt = conn.prepare(
        r#"
        SELECT key, black_hex, white_hex, side, empties, legal_moves, terminal, reach_count, value_black
        FROM positions
        ORDER BY empties DESC, key ASC
        "#,
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, Option<i64>>(8)?,
        ))
    })?;
    for row in rows {
        let (key, black_hex, white_hex, side, empties, legal_moves, terminal, reach, value) = row?;
        writeln!(
            writer,
            "{key},{black_hex},{white_hex},{side},{empties},{},{terminal},{reach},{}",
            legal_moves
                .map(|v| v.to_string())
                .unwrap_or_else(String::new),
            value.map(|v| v.to_string()).unwrap_or_else(String::new)
        )?;
    }
    println!("exported: {}", output_path.display());
    Ok(())
}

#[derive(Debug)]
struct Config {
    db_path: PathBuf,
    command: Command,
}

#[derive(Debug)]
enum Command {
    Init {
        reset: bool,
    },
    Run {
        batch: usize,
        max_positions: Option<u64>,
        max_seconds: Option<u64>,
    },
    Status,
    Perft {
        depth: u8,
        divide: bool,
    },
    Retrograde {
        batch: usize,
        max_passes: Option<u64>,
    },
    ExportCsv {
        output_path: PathBuf,
    },
    Help,
}

fn parse_args() -> Result<Config> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let mut db_path = PathBuf::from(DEFAULT_DB);

    let mut i = 0;
    while i < args.len() {
        if args[i] == "--db" {
            let value = args.get(i + 1).context("--db requires a path")?.to_string();
            db_path = PathBuf::from(value);
            args.drain(i..=i + 1);
        } else {
            i += 1;
        }
    }

    let command = match args.first().map(String::as_str) {
        None | Some("help") | Some("--help") | Some("-h") => Command::Help,
        Some("init") => Command::Init {
            reset: args.iter().any(|arg| arg == "--reset"),
        },
        Some("run") => Command::Run {
            batch: parse_usize_option(&args, "--batch")?.unwrap_or(100),
            max_positions: parse_u64_option(&args, "--max-positions")?,
            max_seconds: parse_u64_option(&args, "--max-seconds")?,
        },
        Some("status") => Command::Status,
        Some("perft") => Command::Perft {
            depth: parse_u8_option(&args, "--depth")?.unwrap_or(6),
            divide: args.iter().any(|arg| arg == "--divide"),
        },
        Some("retrograde") => Command::Retrograde {
            batch: parse_usize_option(&args, "--batch")?.unwrap_or(1000),
            max_passes: parse_u64_option(&args, "--max-passes")?,
        },
        Some("export-csv") => {
            let output_path = parse_string_option(&args, "--out")?
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("positions.csv"));
            Command::ExportCsv { output_path }
        }
        Some(other) => bail!("unknown command: {other}"),
    };

    Ok(Config { db_path, command })
}

fn parse_string_option(args: &[String], name: &str) -> Result<Option<String>> {
    for (index, arg) in args.iter().enumerate() {
        if arg == name {
            return args
                .get(index + 1)
                .cloned()
                .map(Some)
                .with_context(|| format!("{name} requires a value"));
        }
    }
    Ok(None)
}

fn parse_u64_option(args: &[String], name: &str) -> Result<Option<u64>> {
    parse_string_option(args, name)?
        .map(|value| {
            value
                .parse::<u64>()
                .with_context(|| format!("invalid integer for {name}: {value}"))
        })
        .transpose()
}

fn parse_usize_option(args: &[String], name: &str) -> Result<Option<usize>> {
    parse_string_option(args, name)?
        .map(|value| {
            value
                .parse::<usize>()
                .with_context(|| format!("invalid integer for {name}: {value}"))
        })
        .transpose()
}

fn parse_u8_option(args: &[String], name: &str) -> Result<Option<u8>> {
    parse_string_option(args, name)?
        .map(|value| {
            value
                .parse::<u8>()
                .with_context(|| format!("invalid integer for {name}: {value}"))
        })
        .transpose()
}

fn print_help() {
    println!(
        r#"Othello full-search database builder

USAGE:
  othello_full_search [--db PATH] <command> [options]

COMMANDS:
  init [--reset]
      Create the SQLite database and insert the initial position.

  run [--batch N] [--max-positions N] [--max-seconds N]
      Expand every legal child from the frontier. No alpha-beta, no heuristic pruning.
      Ctrl+C is safe; committed work resumes on the next run.

  status
      Show database progress.

  retrograde [--batch N] [--max-passes N]
      Fill value_black when all children of a position already have exact values.
      Black maximizes value_black, White minimizes it.

  perft [--depth N] [--divide]
      Count legal game-tree leaves to a fixed depth without using the database.

  export-csv [--out PATH]
      Export positions for external research tools.

NOTES:
  The 8x8 Othello game tree is astronomically large. This program is designed
  to be correct and resumable, not to make the complete search small.
"#
    );
}

fn main() -> Result<()> {
    let config = parse_args()?;
    match config.command {
        Command::Init { reset } => init_command(&config.db_path, reset),
        Command::Run {
            batch,
            max_positions,
            max_seconds,
        } => run_command(&config.db_path, batch, max_positions, max_seconds),
        Command::Status => status_command(&config.db_path),
        Command::Perft { depth, divide } => perft_command(depth, divide),
        Command::Retrograde { batch, max_passes } => {
            retrograde_command(&config.db_path, batch, max_passes)
        }
        Command::ExportCsv { output_path } => export_positions_csv(&config.db_path, &output_path),
        Command::Help => {
            print_help();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_position_has_four_moves() {
        let board = Board::initial();
        let names: Vec<String> = bits(board.legal_moves()).map(square_name).collect();
        assert_eq!(names, vec!["D3", "C4", "F5", "E6"]);
    }

    #[test]
    fn known_initial_perft_counts() {
        let board = Board::initial();
        let expected = [
            (0, 1u64),
            (1, 4),
            (2, 12),
            (3, 56),
            (4, 244),
            (5, 1396),
            (6, 8200),
        ];
        for (depth, count) in expected {
            assert_eq!(perft(board, depth), BigUint::from(count));
        }
    }

    #[test]
    fn move_application_flips_discs() {
        let board = Board::initial().apply_move(19); // D3
        assert_eq!(board.black.count_ones(), 4);
        assert_eq!(board.white.count_ones(), 1);
        assert_eq!(board.side, Side::White);
    }
}
