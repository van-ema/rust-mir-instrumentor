use apportionment::{process, ApportionmentInput, CandidateVotes, ListVotes};
use std::hint::black_box;

const MAX_LISTS: usize = 16;
const MAX_CANDIDATES_PER_LIST: usize = 64;
const MAX_SEATS: u32 = 64;
const MAX_VOTES: u32 = 200_000;

#[derive(Debug, PartialEq)]
struct FuzzCandidateVotes {
    number: u32,
    votes: u32,
}

impl CandidateVotes for FuzzCandidateVotes {
    type CandidateNumber = u32;

    fn number(&self) -> Self::CandidateNumber {
        self.number
    }

    fn votes(&self) -> u32 {
        self.votes
    }
}

#[derive(Debug, PartialEq)]
struct FuzzListVotes {
    number: u32,
    candidate_votes: Vec<FuzzCandidateVotes>,
}

impl ListVotes for FuzzListVotes {
    type Cv = FuzzCandidateVotes;
    type ListNumber = u32;

    fn number(&self) -> Self::ListNumber {
        self.number
    }

    fn candidate_votes(&self) -> &[Self::Cv] {
        &self.candidate_votes
    }
}

struct FuzzApportionmentInput {
    seats: u32,
    list_votes: Vec<FuzzListVotes>,
}

impl ApportionmentInput for FuzzApportionmentInput {
    type List = FuzzListVotes;

    fn number_of_seats(&self) -> u32 {
        self.seats
    }

    fn list_votes(&self) -> &[Self::List] {
        &self.list_votes
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    idx: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, idx: 0 }
    }

    fn byte(&mut self) -> u8 {
        if self.idx >= self.data.len() {
            0
        } else {
            let out = self.data[self.idx];
            self.idx += 1;
            out
        }
    }

    fn bounded_usize(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.byte() as usize) % bound
        }
    }

    fn bounded_u32(&mut self, bound: u32) -> u32 {
        if bound == 0 {
            0
        } else {
            self.read_u32() % bound
        }
    }

    fn read_u32(&mut self) -> u32 {
        let b0 = self.byte() as u32;
        let b1 = self.byte() as u32;
        let b2 = self.byte() as u32;
        let b3 = self.byte() as u32;
        b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
    }
}

fn build_input(cursor: &mut Cursor<'_>) -> FuzzApportionmentInput {
    let seats = 1 + cursor.bounded_u32(MAX_SEATS);
    let list_count = 1 + cursor.bounded_usize(MAX_LISTS);
    let mut list_votes = Vec::with_capacity(list_count);

    for list_idx in 0..list_count {
        let candidate_count = cursor.bounded_usize(MAX_CANDIDATES_PER_LIST + 1);
        let mut candidate_votes = Vec::with_capacity(candidate_count);
        for candidate_idx in 0..candidate_count {
            let votes = cursor.bounded_u32(MAX_VOTES + 1);
            candidate_votes.push(FuzzCandidateVotes {
                number: (candidate_idx as u32).saturating_add(1),
                votes,
            });
        }
        list_votes.push(FuzzListVotes {
            number: (list_idx as u32).saturating_add(1),
            candidate_votes,
        });
    }

    FuzzApportionmentInput { seats, list_votes }
}

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    let mut cursor = Cursor::new(&data);
    let input = build_input(&mut cursor);
    let result = process(&input);

    match result {
        Ok(output) => {
            let mut sink = output.seat_assignment.full_seats as u64
                ^ ((output.seat_assignment.residual_seats as u64) << 8)
                ^ ((output.candidate_nomination.chosen_candidates.len() as u64) << 16)
                ^ ((output.candidate_nomination.list_candidate_nomination.len() as u64) << 24);
            for standing in output.seat_assignment.final_standing.iter().take(8) {
                sink ^= (standing.total_seats as u64) << (standing.list_number % 16);
            }
            black_box(sink);
        }
        Err(err) => {
            black_box(err);
        }
    }
}
