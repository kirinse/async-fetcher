// Copyright 2021-2022 System76 <info@system76.com>
// SPDX-License-Identifier: MPL-2.0

use crate::get::FetchLocation;
use crate::*;

pub async fn get_many<Data: Send + Sync + 'static>(
    fetcher: Arc<Fetcher<Data>>,
    to: Arc<Path>,
    uris: Arc<[Box<str>]>,
    offset: u64,
    length: u64,
    mut modified: Option<HttpDate>,
    extra: Arc<Data>,
) -> Result<(), Error> {
    let parent = to.parent().ok_or(Error::Parentless)?;
    let filename = to.file_name().ok_or(Error::Nameless)?;

    let mut buf = [0u8; 20];

    let FetchLocation { ref mut file, .. } =
        FetchLocation::create(to.clone(), None, offset != 0).await?;

    let concurrent_fetches = fetcher.connections_per_file as usize;

    let to_ = to.clone();
    let parts =
        stream::iter(range::generate(length, fetcher.max_part_size.into(), offset).enumerate())
            // Generate a future for fetching each part that a range describes.
            .map(move |(partn, (range_start, range_end))| {
                let uri = uris[partn % uris.len()].clone();

                let part_path = {
                    let mut new_filename = filename.to_os_string();
                    new_filename.push(&[".part", partn.numtoa_str(10, &mut buf)].concat());
                    parent.join(new_filename)
                };

                let fetcher = fetcher.clone();
                let to = to_.clone();
                let extra = extra.clone();

                async move {
                    let range = range::to_string(range_start, Some(range_end));

                    fetcher.send(|| {
                        (
                            to.clone(),
                            extra.clone(),
                            FetchEvent::PartFetching(partn as u64),
                        )
                    });

                    let result = crate::get(
                        fetcher.clone(),
                        Request::get(&*uri).header("range", range.as_str()),
                        FetchLocation::create(
                            part_path.into(),
                            Some(range_end - range_start),
                            false,
                        )
                        .await?,
                        to.clone(),
                        &mut modified,
                        extra.clone(),
                    )
                    .await;

                    fetcher.send(|| (to, extra.clone(), FetchEvent::PartFetched(partn as u64)));

                    result
                }
            })
            // Ensure that only this many connections are happenning concurrently at a
            // time
            .buffered(concurrent_fetches);

    concatenator(file, parts).await?;

    if let Some(modified) = modified {
        crate::time::update_modified(&to, modified)?;
    }

    Ok(())
}