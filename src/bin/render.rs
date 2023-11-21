use cgmath::Vector2;
use clap::Parser;
use egui::Vec2;
use half::f16;
use image::{codecs::png::PngEncoder, ImageBuffer, Rgba};
use indicatif::{ProgressBar, ProgressIterator, ProgressStyle};
use std::{
    fs::File,
    path::PathBuf,
    time::{Duration, Instant},
};
use web_splats::{
    GaussianRenderer, PCDataType, PointCloud, Scene, SceneCamera, Split, WGPUContext,
};
use wgpu::SubmissionIndex;

#[derive(Debug, Parser)]
#[command(author, version)]
#[command(about = "Dataset offline renderer. Renders to PNG files", long_about = None)]
struct Opt {
    /// input file
    input: PathBuf,

    /// scene json file
    scene: PathBuf,

    /// image output directory
    img_out: PathBuf,

    /// maximum allowed Spherical Harmonics (SH) degree
    #[arg(long, default_value_t = 3)]
    max_sh_deg: u32,
}

async fn render_views(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &mut GaussianRenderer,
    pc: &mut PointCloud,
    cameras: Vec<SceneCamera>,
    img_out: &PathBuf,
    split: &str,
) {
    let img_out = img_out.join(&split);
    println!("saving images to '{}'", img_out.to_string_lossy());
    std::fs::create_dir_all(img_out.clone()).unwrap();

    let pb = ProgressBar::new(cameras.len() as u64);
    let pb_style = ProgressStyle::with_template(
        "{msg} {spinner:.green} [{bar:.cyan/blue}] {pos}/{len} [{elapsed}/{duration}]",
    )
    .unwrap()
    .progress_chars("#>-");
    pb.set_style(pb_style);
    pb.set_message(format!("rendering {split}"));
    let mut durations: Vec<Duration> = Vec::new();
    let mut resolution: Vector2<u32> = Vector2::new(1237, 822);

    // if resolution.x > 1600 {
    //     let s = resolution.x as f32 / 1600.;
    //     resolution.x = 1600;
    //     resolution.y = (resolution.y as f32 / s) as u32;
    // }

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("render texture"),
        size: wgpu::Extent3d {
            width: resolution.x,
            height: resolution.y,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: renderer.color_format(),
        usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
    for (i, s) in cameras.iter().enumerate() {
        // let mut resolution: Vector2<u32> = Vector2::new(s.width, s.height);

        renderer.render(
            device,
            queue,
            &target_view,
            &pc,
            s.clone().into(),
            resolution,
        );

        renderer.stopwatch.reset();

        let times = renderer.stopwatch.take_measurements(&device, &queue).await;
        let img = download_texture(&target, device, queue).await;

        let mut out_file = File::create(img_out.join(format!("{i:0>5}.png"))).unwrap();
        let encoder = PngEncoder::new_with_quality(
            &mut out_file,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::NoFilter,
        );
        img.write_with_encoder(encoder).unwrap();
    }
}

#[pollster::main]
async fn main() {
    #[cfg(not(target_arch = "wasm32"))]
    env_logger::init();
    let opt = Opt::parse();

    println!("reading scene file '{}'", opt.scene.to_string_lossy());

    // TODO this is suboptimal as it is never closed
    let ply_file = File::open(&opt.input).unwrap();
    let scene_file = File::open(opt.scene).unwrap();

    let scene = Scene::from_json(scene_file).unwrap();

    let wgpu_context = WGPUContext::new_instance().await;
    let device = &wgpu_context.device;
    let queue = &wgpu_context.queue;

    println!("reading point cloud file '{}'", opt.input.to_string_lossy());
    let pc_data_type = match opt
        .input
        .extension()
        .expect("file has no extension!")
        .to_str()
        .unwrap()
    {
        "ply" => PCDataType::PLY,
        #[cfg(feature = "npz")]
        "npz" => PCDataType::NPZ,
        ext => panic!("unsupported file type '{ext}"),
    };
    let mut pc = match pc_data_type {
        PCDataType::PLY => PointCloud::load_ply(
            &wgpu_context.device,
            &wgpu_context.queue,
            ply_file,
            Some(opt.max_sh_deg),
        )
        .unwrap(),
        #[cfg(feature = "npz")]
        PCDataType::NPZ => PointCloud::load_npz(
            &wgpu_context.device,
            &wgpu_context.queue,
            ply_file,
            Some(opt.max_sh_deg),
        )
        .unwrap(),
    };

    let mut renderer = GaussianRenderer::new(
        device,
        wgpu::TextureFormat::Rgba32Float,
        pc.sh_deg(),
        pc_data_type == PCDataType::PLY,
    );

    render_views(
        device,
        queue,
        &mut renderer,
        &mut pc,
        scene.cameras(Some(Split::Test)),
        &opt.img_out,
        "test",
    )
    .await;
    // render_views(
    //     device,
    //     queue,
    //     &mut renderer,
    //     &mut pc,
    //     scene.cameras(Some(Split::Train)),
    //     &opt.img_out,
    //     "train",
    // )
    // .await;

    println!("done!");
}

pub async fn download_texture(
    texture: &wgpu::Texture,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
) -> ImageBuffer<Rgba<u8>, Vec<u8>> {
    let texture_format = texture.format();

    let texel_size: u32 = texture_format.block_size(None).unwrap();
    let fb_size = texture.size();
    let align: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT - 1;
    let bytes_per_row = (texel_size * fb_size.width) + align & !align;

    let output_buffer_size = (bytes_per_row * fb_size.height) as wgpu::BufferAddress;

    let output_buffer_desc = wgpu::BufferDescriptor {
        size: output_buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        label: Some("texture download buffer"),
        mapped_at_creation: false,
    };
    let download_buffer = device.create_buffer(&output_buffer_desc);

    let mut encoder: wgpu::CommandEncoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("download frame buffer encoder"),
        });

    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::ImageCopyBufferBase {
            buffer: &download_buffer,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(fb_size.height),
            },
        },
        fb_size,
    );
    let sub_idx = queue.submit(std::iter::once(encoder.finish()));

    let mut image = {
        let data = web_splats::download_buffer(device, &download_buffer, Some(sub_idx)).await;

        let buf: Vec<u8> = data
            .to_vec()
            .chunks_exact(4)
            .map(|c| {
                let b = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                (b.clamp(0., 1.) * 255.) as u8
            })
            .collect();

        ImageBuffer::<Rgba<_>, _>::from_raw(bytes_per_row / texel_size, fb_size.height, buf)
            .unwrap()
    };

    download_buffer.unmap();

    return image::imageops::crop(&mut image, 0, 0, fb_size.width, fb_size.height).to_image();
}
