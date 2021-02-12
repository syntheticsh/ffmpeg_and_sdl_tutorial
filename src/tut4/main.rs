extern crate ffmpeg_next as ffmpeg;

use anyhow::{anyhow, Context as ErrorContext, Result};
use ffmpeg::format::{input, sample::Type as AudioType, Pixel, Sample};
use ffmpeg::frame::Audio;
use ffmpeg::media::Type;
use ffmpeg::software::resampling::{context::Context as AudioContext, flag::Flags as AudioFlags};
use ffmpeg::software::scaling::{context::Context, flag::Flags};
use ffmpeg::util::frame::video::Video;
use sdl2::audio::{AudioSpec, AudioSpecDesired};
use sdl2::event::Event;
use sdl2::keyboard::Keycode;
use sdl2::pixels::PixelFormatEnum;
use sdl2::render::{TextureCreator, WindowCanvas};
use sdl2::video::WindowContext;
use std::env;

enum DecodeResult {
    Audio(Audio),
    Video(Video),
}

#[allow(clippy::too_many_lines)]
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
        // даелее мы смотрим доступные потоки, конкретно тут мы ищем "лучший" видео поток
        let input = ictx
            .streams()
            .best(Type::Video)
            .ok_or(ffmpeg::Error::StreamNotFound)?;
        // дальше находим лучший аудио поток
        let a_input = ictx
            .streams()
            .best(Type::Audio)
            .ok_or(ffmpeg::Error::StreamNotFound)?;

        let video_stream_index = input.index();
        let audio_stream_index = a_input.index();

        // находим декодер (кодек) по id видео потока
        // под копотом в функции .video() вызывает avcodec_find_decoder()
        // и потом открывается сам коде через avcodec_open2()
        let mut decoder = input.codec().decoder().video()?;

        // находим так же и кодек аудио
        let mut a_decoder = a_input.codec().decoder().audio()?;

        // по сути какой-то синглтон который следит за тем что бы у нас не было несколько контекстов
        let sdl_context = sdl2::init().map_err(|e| anyhow!(e))?;
        // по сути это SDL_init(SDL_INIT_VIDEO)
        let video_subsystem = sdl_context.video().map_err(|e| anyhow!(e))?;
        let audio_subsystem = sdl_context.audio().map_err(|e| anyhow!(e))?;

        // создаём окно в котором будем отображать информацию
        let window = video_subsystem
            .window("rust-sdl2 demo: Video", decoder.width(), decoder.height())
            .position_centered()
            .opengl()
            .build()
            .context("couldn't create window")?;

        // создаём канвас в окне SDL_CreateRenderer()
        let mut canvas = window
            .into_canvas()
            .build()
            .context("couldn't create canvas")?;
        let texture_creator = canvas.texture_creator();

        let desired_spec = AudioSpecDesired {
            freq: Some(a_decoder.rate() as i32),
            channels: Some(a_decoder.channels() as u8),
            samples: Some(4),
        };

        let audio_device = audio_subsystem
            .open_queue::<i16, _>(None, &desired_spec)
            .map_err(|e| anyhow!(e))?;

        let audio_started = false;

        let break_flag = std::sync::Arc::new(std::sync::Mutex::new(false));

        let mut event_pump = sdl_context.event_pump().map_err(|e| anyhow!(e))?;

        let (decoded_tx, decoded_rx) = std::sync::mpsc::channel();

        let ph = packet_receiver(
            decoder,
            a_decoder,
            decoded_tx,
            ictx,
            video_stream_index,
            audio_stream_index,
            break_flag.clone(),
        );

        loop {
            let res = decoded_rx.recv()?;
            match res {
                DecodeResult::Audio(frame_to_play) => {
                    audio_device.queue(unsafe { frame_to_play.data(0).align_to::<i16>() }.1);
                    if !audio_started {
                        audio_device.resume();
                    }
                }
                DecodeResult::Video(mut frame_to_display) => {
                    draw_frame(&mut frame_to_display, &mut canvas, &texture_creator).unwrap();
                }
            }
            if let Some(event) = event_pump.poll_event() {
                match event {
                    Event::Quit { .. }
                    | Event::KeyDown {
                        keycode: Some(Keycode::Escape),
                        ..
                    } => {
                        break_flag.lock().map(|_| true);
                        break;
                    }
                    _ => {}
                }
            }
        }

        ph.join();
    }

    Ok(())
}

fn packet_receiver(
    mut decoder: ffmpeg::codec::decoder::Video,
    mut a_decoder: ffmpeg::codec::decoder::Audio,
    decoded_tx: std::sync::mpsc::Sender<DecodeResult>,
    mut ictx: ffmpeg::format::context::Input,
    video_stream_index: usize,
    audio_stream_index: usize,
    break_flag: std::sync::Arc<std::sync::Mutex<bool>>,
) -> std::thread::JoinHandle<Result<()>> {
    std::thread::spawn(move || -> Result<()> {
        let (a_tx, a_rx) = std::sync::mpsc::channel();
        let (v_tx, v_rx) = std::sync::mpsc::channel();

        let va = audio_thread(a_decoder, a_rx, decoded_tx.clone());
        let vh = video_thread(decoder, v_rx, decoded_tx);

        // читаем все пакеты из потока через av_read_frame()
        for (stream, packet) in ictx.packets() {
            let packet = std::sync::Arc::new(packet);
            // если пакет относится к видео
            if stream.index() == video_stream_index {
                v_tx.send(packet).unwrap_or(());
            } else if stream.index() == audio_stream_index {
                a_tx.send(packet).unwrap_or(());
            }

            if *break_flag.lock().unwrap() {
                break;
            }
        }

        drop(a_tx);
        drop(v_tx);

        va.join();
        vh.join();

        Ok(())
    })
}

fn video_thread(
    mut decoder: ffmpeg::codec::decoder::Video,
    v_rx: std::sync::mpsc::Receiver<std::sync::Arc<ffmpeg::codec::packet::Packet>>,
    result_tx: std::sync::mpsc::Sender<DecodeResult>,
) -> std::thread::JoinHandle<Result<()>> {
    std::thread::spawn(move || -> Result<()> {
        // определяем из какого формата в какой переводим
        let mut context = Context::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            Pixel::YUV420P,
            decoder.width(),
            decoder.height(),
            Flags::BILINEAR,
        )?;

        // функция для докодирования фреймов и записи их в файл
        let mut receive_and_process_decoded_frames =
            |decoder: &mut ffmpeg::decoder::Video| -> Result<(), ffmpeg::Error> {
                // здесь происходит аллокация пустого фрейма через av_frame_alloc()
                let mut decoded = Video::empty();
                // пытаемся получить готовый фрейм из декодера через avcodec_receive_frame()
                while decoder.receive_frame(&mut decoded).is_ok() {
                    // здесь происходит аллокация пустого фрейма куда мы поместим модифицированный фрейм
                    let mut frame_to_display = Video::empty();
                    // переводим фрейм в нужный формат sws_scale()
                    context.run(&decoded, &mut frame_to_display)?;
                    //let frame_to_display = std::sync::Arc::new(frame_to_display);
                    result_tx
                        .send(DecodeResult::Video(frame_to_display))
                        .unwrap_or(());
                }
                Ok(())
            };

        while let Ok(packet) = v_rx.recv() {
            // посылаем пакет в декодер avcodec_send_packet()
            decoder.send_packet(&*packet)?;
            receive_and_process_decoded_frames(&mut decoder)?;
        }
        decoder.send_eof()?;

        Ok(())
    })
}

fn audio_thread(
    mut a_decoder: ffmpeg::codec::decoder::Audio,
    a_rx: std::sync::mpsc::Receiver<std::sync::Arc<ffmpeg::codec::packet::Packet>>,
    result_tx: std::sync::mpsc::Sender<DecodeResult>,
) -> std::thread::JoinHandle<Result<()>> {
    std::thread::spawn(move || -> Result<()> {
        let mut a_context = AudioContext::get(
            a_decoder.format(),
            a_decoder.channel_layout(),
            a_decoder.rate(),
            Sample::I16(AudioType::Packed),
            a_decoder.channel_layout(),
            a_decoder.rate(),
        )?;

        let mut receive_and_process_decoded_frames =
            |decoder: &mut ffmpeg::decoder::Audio| -> Result<(), ffmpeg::Error> {
                let mut decoded = Audio::empty();
                while decoder.receive_frame(&mut decoded).is_ok() {
                    let mut frame_to_play = Audio::empty();
                    a_context.run(&decoded, &mut frame_to_play)?;
                    //let frame_to_play = std::sync::Arc::new(frame_to_play);
                    result_tx
                        .send(DecodeResult::Audio(frame_to_play))
                        .unwrap_or(())
                }
                Ok(())
            };

        while let Ok(packet) = a_rx.recv() {
            a_decoder.send_packet(&*packet)?;
            receive_and_process_decoded_frames(&mut a_decoder)?;
        }
        a_decoder.send_eof();

        Ok(())
    })
}

fn draw_frame(
    frame: &mut Video,
    canvas: &mut WindowCanvas,
    texture_creator: &TextureCreator<WindowContext>,
) -> Result<()> {
    let mut texture = texture_creator
        .create_texture_streaming(PixelFormatEnum::YV12, frame.width(), frame.height())
        .context("couldn't create texture")?;
    texture
        .with_lock(None, |buffer: &mut [u8], _: usize| {
            let mut index: usize = 0;
            for (i, byte) in frame.data(0).iter().enumerate() {
                buffer[i] = *byte;
                index += 1;
            }

            for byte in frame.data(2) {
                buffer[index] = *byte;
                index += 1;
            }

            for byte in frame.data(1) {
                buffer[index] = *byte;
                index += 1;
            }
        })
        .map_err(|e| anyhow!(e))?;
    canvas.copy(&texture, None, None).map_err(|e| anyhow!(e))?;
    canvas.present();
    Ok(())
}
