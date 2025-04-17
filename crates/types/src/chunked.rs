use eyre::Result;

pub struct ChunkedIterator<I>
where
    I: Iterator<Item = Result<Vec<u8>>>,
{
    source: I,
    buffer: Vec<u8>,
    chunk_size: usize,
    finished: bool,
}

impl<I> ChunkedIterator<I>
where
    I: Iterator<Item = Result<Vec<u8>>>,
{
    pub fn new(source: I, chunk_size: usize) -> Self {
        Self {
            source,
            buffer: Vec::new(),
            chunk_size,
            finished: false,
        }
    }
}

impl<I> Iterator for ChunkedIterator<I>
where
    I: Iterator<Item = Result<Vec<u8>>>,
{
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished && self.buffer.is_empty() {
            return None;
        }

        // Fill the buffer until we have enough for a chunk
        while self.buffer.len() < self.chunk_size {
            match self.source.next() {
                Some(Ok(bytes)) => self.buffer.extend(bytes),
                Some(Err(e)) => {
                    self.finished = true;
                    return Some(Err(e));
                }
                None => {
                    self.finished = true;
                    break;
                }
            }
        }

        if self.buffer.is_empty() {
            None
        } else {
            let chunk_size = if self.finished {
                self.buffer.len()
            } else {
                self.chunk_size.min(self.buffer.len())
            };

            Some(Ok(self.buffer.drain(..chunk_size).collect()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eyre::eyre;

    #[test]
    fn test_exact_chunks() -> Result<()> {
        let data = vec![Ok(vec![1, 2]), Ok(vec![3, 4]), Ok(vec![5, 6])];
        let mut iter = ChunkedIterator::new(data.into_iter(), 3);

        assert_eq!(iter.next().transpose()?, Some(vec![1, 2, 3]));
        assert_eq!(iter.next().transpose()?, Some(vec![4, 5, 6]));
        assert!(iter.next().is_none());
        Ok(())
    }

    #[test]
    fn test_partial_final_chunk() -> Result<()> {
        let data = vec![Ok(vec![1, 2]), Ok(vec![3, 4]), Ok(vec![5])];
        let mut iter = ChunkedIterator::new(data.into_iter(), 3);

        assert_eq!(iter.next().transpose()?, Some(vec![1, 2, 3]));
        assert_eq!(iter.next().transpose()?, Some(vec![4, 5]));
        assert!(iter.next().is_none());
        Ok(())
    }

    #[test]
    fn test_error_propagation() {
        let data = vec![Ok(vec![1, 2]), Err(eyre!("test error")), Ok(vec![3, 4])];
        let mut iter = ChunkedIterator::new(data.into_iter(), 2);

        assert!(iter.next().unwrap().is_ok());
        assert!(iter.next().unwrap().is_err());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_empty_input() {
        let data: Vec<Result<Vec<u8>>> = vec![];
        let mut iter = ChunkedIterator::new(data.into_iter(), 2);

        assert!(iter.next().is_none());
    }

    #[test]
    fn test_variable_input_sizes() -> Result<()> {
        let data = vec![Ok(vec![1]), Ok(vec![2, 3, 4]), Ok(vec![5, 6])];
        let mut iter = ChunkedIterator::new(data.into_iter(), 2);

        assert_eq!(iter.next().transpose()?, Some(vec![1, 2]));
        assert_eq!(iter.next().transpose()?, Some(vec![3, 4]));
        assert_eq!(iter.next().transpose()?, Some(vec![5, 6]));
        assert!(iter.next().is_none());
        Ok(())
    }

    #[test]
    fn test_chunk_size_larger_than_total_input() -> Result<()> {
        let data = vec![Ok(vec![1]), Ok(vec![2, 3])];
        let mut iter = ChunkedIterator::new(data.into_iter(), 5);

        assert_eq!(iter.next().transpose()?, Some(vec![1, 2, 3]));
        assert!(iter.next().is_none());
        Ok(())
    }
}
