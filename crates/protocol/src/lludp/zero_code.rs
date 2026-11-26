use super::error::LLUdpError;

pub(crate) fn encode(mut input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    while let Some((&byte, rest)) = input.split_first() {
        if byte != 0 {
            output.push(byte);
            input = rest;
            continue;
        }

        let mut run = 1usize;
        input = rest;
        while run < usize::from(u8::MAX) {
            let Some((&0, next)) = input.split_first() else {
                break;
            };
            run += 1;
            input = next;
        }
        output.extend_from_slice(&[0, run as u8]);
    }
    output
}

fn read_zero_count(input: &[u8], cursor: &mut usize) -> Result<usize, LLUdpError> {
    let count = *input.get(*cursor).ok_or(LLUdpError::InvalidZeroCode)?;
    *cursor += 1;
    if count != 0 {
        return Ok(usize::from(count));
    }

    let extension = *input.get(*cursor).ok_or(LLUdpError::InvalidZeroCode)?;
    *cursor += 1;
    Ok(usize::from(u8::MAX) + usize::from(extension))
}

pub(crate) fn decode_prefix(input: &[u8], mut output: &mut [u8]) -> Result<usize, LLUdpError> {
    let total = output.len();
    let mut input_cursor = 0;
    while !output.is_empty() {
        let Some(&byte) = input.get(input_cursor) else {
            break;
        };
        input_cursor += 1;
        if byte == 0 {
            let count = read_zero_count(input, &mut input_cursor)?.min(output.len());
            if let Some(zeros) = output.split_off_mut(..count) {
                zeros.fill(0);
            }
        } else if let Some(slot) = output.split_off_first_mut() {
            *slot = byte;
        }
    }
    Ok(total - output.len())
}

#[cfg(test)]
pub(crate) fn decode(input: &[u8], max_output_size: usize) -> Result<Vec<u8>, LLUdpError> {
    let decoded_len = decoded_len(input, max_output_size)?;
    let mut output = Vec::with_capacity(decoded_len);
    decode_into(input, &mut output, decoded_len)?;
    Ok(output)
}

#[cfg(test)]
pub(crate) fn decoded_len(input: &[u8], max_output_size: usize) -> Result<usize, LLUdpError> {
    let mut cursor = 0;
    let mut output_len = 0usize;
    while cursor < input.len() {
        let byte = input[cursor];
        cursor += 1;
        if byte == 0 {
            let count = read_zero_count(input, &mut cursor)?;
            let output_size = output_len.checked_add(count).ok_or(LLUdpError::InvalidZeroCode)?;
            if output_size > max_output_size {
                return Err(LLUdpError::InvalidZeroCode);
            }
            output_len = output_size;
        } else {
            if output_len == max_output_size {
                return Err(LLUdpError::InvalidZeroCode);
            }
            output_len += 1;
        }
    }
    Ok(output_len)
}

pub(crate) fn decode_into(input: &[u8], output: &mut Vec<u8>, expected_len: usize) -> Result<(), LLUdpError> {
    output.clear();
    output.reserve_exact(expected_len);
    let mut cursor = 0;
    while let Some(&byte) = input.get(cursor) {
        cursor += 1;
        if byte == 0 {
            let count = read_zero_count(input, &mut cursor)?;
            let output_size = output.len().checked_add(count).ok_or(LLUdpError::InvalidZeroCode)?;
            if output_size > expected_len {
                return Err(LLUdpError::InvalidZeroCode);
            }
            output.resize(output_size, 0);
        } else {
            if output.len() == expected_len {
                return Err(LLUdpError::InvalidZeroCode);
            }
            output.push(byte);
        }
    }
    if output.len() != expected_len {
        return Err(LLUdpError::InvalidZeroCode);
    }
    Ok(())
}

pub(crate) fn decode_to_slice(input: &[u8], mut output: &mut [u8]) -> Result<usize, LLUdpError> {
    let total = output.len();
    let mut cursor = 0;
    while let Some(&byte) = input.get(cursor) {
        cursor += 1;
        if byte == 0 {
            let count = read_zero_count(input, &mut cursor)?;
            let Some(zeros) = output.split_off_mut(..count) else {
                return Err(LLUdpError::InvalidZeroCode);
            };
            zeros.fill(0);
        } else {
            let Some(slot) = output.split_off_first_mut() else {
                return Err(LLUdpError::InvalidZeroCode);
            };
            *slot = byte;
        }
    }
    Ok(total - output.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_runs_split_at_limit() {
        let input = vec![0; 256];
        let encoded = encode(&input);
        assert_eq!(encoded, [0, 255, 0, 1]);
        assert_eq!(decode(&encoded, input.len()), Ok(input));
        assert_eq!(decode(&[0], usize::MAX), Err(LLUdpError::InvalidZeroCode));
        assert_eq!(decode(&[0, 0], usize::MAX), Err(LLUdpError::InvalidZeroCode));
        assert_eq!(decode(&[0, 0, 0], 255), Ok(vec![0; 255]));
        assert_eq!(decode(&[0, 0, 1], 256), Ok(vec![0; 256]));
        assert_eq!(decode(&[0, 0, 255], 510), Ok(vec![0; 510]));
        assert_eq!(decode(&[0, 0, 255], 509), Err(LLUdpError::InvalidZeroCode));
        assert_eq!(decode(&[0, u8::MAX], usize::from(u8::MAX) - 1), Err(LLUdpError::InvalidZeroCode));
        assert_eq!(decode(&[1, 2], 1), Err(LLUdpError::InvalidZeroCode));
    }

    #[test]
    fn prefix_decode_is_bounded() {
        let mut output = [0; 4];
        assert_eq!(decode_prefix(&[0, u8::MAX, 0], &mut output), Ok(4));
        assert_eq!(output, [0; 4]);
        assert_eq!(decode_prefix(&[0], &mut output), Err(LLUdpError::InvalidZeroCode));
        assert_eq!(decode_prefix(&[0, 0], &mut output), Err(LLUdpError::InvalidZeroCode));
        assert_eq!(decode_prefix(&[0, 0, 1], &mut output), Ok(4));
    }
}
