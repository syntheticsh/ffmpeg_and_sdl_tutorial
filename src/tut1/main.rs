extern crate ffmpeg_next as ffmpeg;

use anyhow::{anyhow, Context as AContext, Result};
use ffmpeg::format::{input, Pixel};
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context, flag::Flags};
use ffmpeg::util::frame::video::Video;
use std::env;

fn main() -> Result<()> {
    // регистриует все доступные форматы, кодеки и т.д.
    ffmpeg::init().unwrap();

    // открываем указанный input сюда идёт всё то что можно указать через -i
    // по сути читает header файла или подобные действия получает информацию о формате input
    if let Ok(mut ictx) = input(
        &env::args()
            .nth(1)
            .ok_or_else(|| anyhow!("no input specified"))?,
    ) {
        // дамп информации о контексте input'а, тертий параметр не обязательный
        ffmpeg::format::context::input::dump(&ictx, 0, Some(env::args().nth(1).unwrap().as_str()));

        // даелее мы смотрим доступные потоки, конкретно тут мы ищем "лучший" видео поток
        let input = ictx
            .streams()
            .best(Type::Video)
            .ok_or(ffmpeg::Error::StreamNotFound)?;
        let video_stream_index = input.index();

        // находим декодер (кодек) по id видео потока
        // под копотом в функции .video() вызывает avcodec_find_decoder()
        // и потом открывается сам коде через avcodec_open2()
        let mut decoder = input.codec().decoder().video()?;

        // определяем из какого формата в какой переводим
        let mut scaler = Context::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            Pixel::RGB24,
            decoder.width(),
            decoder.height(),
            Flags::BILINEAR,
        )?;

        let mut frame_index = 0;

        // функция для докодирования фреймов и записи их в файл
        let mut receive_and_process_decoded_frames =
            |decoder: &mut ffmpeg::decoder::Video| -> Result<(), ffmpeg::Error> {
                // здесь происходит аллокация пустого фрейма через av_frame_alloc()
                let mut decoded = Video::empty();
                // пытаемся получить готовый фрейм из декодера через avcodec_receive_frame()
                while decoder.receive_frame(&mut decoded).is_ok() {
                    frame_index += 1;

                    if frame_index % 30 != 0 {
                        continue;
                    }

                    // здесь происходит аллокация пустого фрейма куда мы поместим модифицированный фрейм
                    let mut rgb_frame = Video::empty();
                    // переводим фрейм в нужный формат sws_scale()
                    scaler.run(&decoded, &mut rgb_frame)?;
                    save_file(&rgb_frame, frame_index).unwrap();
                }
                Ok(())
            };

        // читаем все пакеты из потока через av_read_frame()
        for (stream, packet) in ictx.packets() {
            // если пакет относится к видео
            if stream.index() == video_stream_index {
                // посылаем пакет в декодер avcodec_send_packet()
                decoder.send_packet(&packet)?;
                receive_and_process_decoded_frames(&mut decoder)?;
            }
        }
        decoder.send_eof()?;
        receive_and_process_decoded_frames(&mut decoder)?;
    }

    Ok(())
}

fn save_file(frame: &Video, index: usize) -> Result<()> {
    let mut buffer: Vec<u8> =
        format!("P6\n{} {}\n255\n", frame.width(), frame.height()).into_bytes();
    buffer.extend_from_slice(frame.data(0));
    image::save_buffer(
        format!("frame{}.jpeg", index),
        &buffer,
        frame.width(),
        frame.height(),
        image::ColorType::Rgb8,
    )
    .context("couldn't save frame")
}
