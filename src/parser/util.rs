use bitvec::BitArr;
use nom::{bits::complete as bit_parsers, combinator::map, IResult};

use crate::parser::obu::ParserContext;

impl ParserContext {
    fn cur_operating_point_idc(&mut self) -> &mut BitArr!(for 12) {
        self.operating_point_idc
            .get_mut(self.operating_point)
            .expect("`operating_point` out of range of `operating_point_idc`")
    }

    fn choose_operating_point(&mut self) {
        todo!()
    }
}

pub(super) fn take_bool_bit(input: (&[u8], usize)) -> IResult<(&[u8], usize), bool> {
    map(bit_parsers::take(1usize), |output: u8| output > 0)(input)
}

pub(super) fn take_zero_bit(input: (&[u8], usize)) -> IResult<(&[u8], usize), ()> {
    take_zero_bits(input, 1)
}

pub(super) fn take_zero_bits(input: (&[u8], usize), bits: usize) -> IResult<(&[u8], usize), ()> {
    map(bit_parsers::tag(0u8, bits), |_| ())(input)
}
