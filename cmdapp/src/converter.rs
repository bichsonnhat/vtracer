use std::collections::HashSet;
use std::path::Path;
use std::{
    fs::File,
    io::{BufWriter, Write},
};

use super::config::{ColorMode, Config, ConverterConfig, Hierarchical};
use super::svg::SvgFile;
use fastrand::Rng;
use visioncortex::color_clusters::{KeyingAction, Runner, RunnerConfig, HIERARCHICAL_MAX};
use visioncortex::{Color, ColorImage, ColorName};

const NUM_UNUSED_COLOR_ITERATIONS: usize = 6;
/// The fraction of pixels in the top/bottom rows of the image that need to be transparent before
/// the entire image will be keyed.
const KEYING_THRESHOLD: f32 = 0.2;

/// Convert an in-memory image into an in-memory SVG
pub fn convert(img: ColorImage, config: Config) -> Result<SvgFile, String> {
    let config = config.into_converter_config();
    match config.color_mode {
        ColorMode::Color => color_image_to_svg(img, config),
        ColorMode::Binary => binary_image_to_svg(img, config),
    }
}

/// Convert an image file into svg file
pub fn convert_image_to_svg(
    input_path: &Path,
    output_path: &Path,
    config: Config,
) -> Result<(), String> {
    let img = read_image(input_path)?;
    let svg = convert(img, config)?;
    write_svg(svg, output_path)
}

fn find_unused_color_in_image(img: &ColorImage) -> Result<Color, String> {
    let mut present: HashSet<(u8, u8, u8)> = HashSet::with_capacity(512);
    for chunk in img.pixels.chunks_exact(4) {
        present.insert((chunk[0], chunk[1], chunk[2]));
    }

    let special_colors = [
        Color::new(255, 0, 0),
        Color::new(0, 255, 0),
        Color::new(0, 0, 255),
        Color::new(255, 255, 0),
        Color::new(0, 255, 255),
        Color::new(255, 0, 255),
    ];
    let mut rng = Rng::new();
    let random_colors =
        (0..NUM_UNUSED_COLOR_ITERATIONS).map(|_| Color::new(rng.u8(..), rng.u8(..), rng.u8(..)));
    for color in special_colors.into_iter().chain(random_colors) {
        if !present.contains(&(color.r, color.g, color.b)) {
            return Ok(color);
        }
    }
    Err(String::from(
        "unable to find unused color in image to use as key",
    ))
}

fn should_key_image(img: &ColorImage) -> bool {
    if img.width == 0 || img.height == 0 {
        return false;
    }

    // Check for transparency at several scanlines
    let threshold = ((img.width * 2) as f32 * KEYING_THRESHOLD) as usize;
    let mut num_transparent_pixels = 0;
    let y_positions = [
        0,
        img.height / 4,
        img.height / 2,
        3 * img.height / 4,
        img.height - 1,
    ];
    for y in y_positions {
        let row_start = y * img.width * 4;
        for alpha_idx in (row_start + 3..row_start + img.width * 4).step_by(4) {
            if img.pixels[alpha_idx] == 0 {
                num_transparent_pixels += 1;
                if num_transparent_pixels >= threshold {
                    return true;
                }
            }
        }
    }

    false
}

fn color_image_to_svg(mut img: ColorImage, config: ConverterConfig) -> Result<SvgFile, String> {
    let width = img.width;
    let height = img.height;

    let key_color = if should_key_image(&img) {
        let key_color = find_unused_color_in_image(&img)?;
        let [kr, kg, kb] = [key_color.r, key_color.g, key_color.b];
        for chunk in img.pixels.chunks_exact_mut(4) {
            if chunk[3] == 0 {
                chunk[0] = kr;
                chunk[1] = kg;
                chunk[2] = kb;
                chunk[3] = 255;
            }
        }
        key_color
    } else {
        // The default color is all zeroes, which is treated by visioncortex as a special value meaning no keying will be applied.
        Color::default()
    };

    let total_pixels = width * height;
    let batch_size = if total_pixels < 25600 { total_pixels } else { 25600 };

    let runner = Runner::new(
        RunnerConfig {
            diagonal: config.layer_difference == 0,
            hierarchical: HIERARCHICAL_MAX,
            batch_size,
            good_min_area: config.filter_speckle_area,
            good_max_area: (width * height),
            is_same_color_a: config.color_precision_loss,
            is_same_color_b: 1,
            deepen_diff: config.layer_difference,
            hollow_neighbours: 1,
            key_color,
            keying_action: if matches!(config.hierarchical, Hierarchical::Cutout) {
                KeyingAction::Keep
            } else {
                KeyingAction::Discard
            },
        },
        img,
    );

    let mut clusters = runner.run();

    match config.hierarchical {
        Hierarchical::Stacked => {}
        Hierarchical::Cutout => {
            let view = clusters.view();
            let image = view.to_color_image();
            let runner = Runner::new(
                RunnerConfig {
                    diagonal: false,
                    hierarchical: 64,
                    batch_size: 25600,
                    good_min_area: 0,
                    good_max_area: (image.width * image.height) as usize,
                    is_same_color_a: 0,
                    is_same_color_b: 1,
                    deepen_diff: 0,
                    hollow_neighbours: 0,
                    key_color,
                    keying_action: KeyingAction::Discard,
                },
                image,
            );
            clusters = runner.run();
        }
    }

    let view = clusters.view();

    let mut svg = SvgFile::new(width, height, config.path_precision);
    svg.paths.reserve(view.clusters_output.len());
    for &cluster_index in view.clusters_output.iter().rev() {
        let cluster = view.get_cluster(cluster_index);
        let paths = cluster.to_compound_path(
            &view,
            false,
            config.mode,
            config.corner_threshold,
            config.length_threshold,
            config.max_iterations,
            config.splice_threshold,
        );
        svg.add_path(paths, cluster.residue_color());
    }

    Ok(svg)
}

fn binary_image_to_svg(img: ColorImage, config: ConverterConfig) -> Result<SvgFile, String> {
    let img = img.to_binary_image(|x| {
        (0.299 * x.r as f32 + 0.587 * x.g as f32 + 0.114 * x.b as f32) < 128.0
    });
    let width = img.width;
    let height = img.height;

    let clusters = img.to_clusters(false);

    let mut svg = SvgFile::new(width, height, config.path_precision);
    svg.paths.reserve(clusters.len());
    for i in 0..clusters.len() {
        let cluster = clusters.get_cluster(i);
        if cluster.size() >= config.filter_speckle_area {
            let paths = cluster.to_compound_path(
                config.mode,
                config.corner_threshold,
                config.length_threshold,
                config.max_iterations,
                config.splice_threshold,
            );
            svg.add_path(paths, Color::color(&ColorName::Black));
        }
    }

    Ok(svg)
}

fn read_image(input_path: &Path) -> Result<ColorImage, String> {
    let img = image::open(input_path);
    let img = match img {
        Ok(file) => file.to_rgba8(),
        Err(_) => return Err(String::from("No image file found at specified input path")),
    };

    let (width, height) = (img.width() as usize, img.height() as usize);
    let img = ColorImage {
        pixels: img.into_raw(),
        width,
        height,
    };

    Ok(img)
}

fn write_svg(svg: SvgFile, output_path: &Path) -> Result<(), String> {
    let out_file = File::create(output_path);
    let out_file = match out_file {
        Ok(file) => file,
        Err(_) => return Err(String::from("Cannot create output file.")),
    };
    let mut out_file = BufWriter::new(out_file);

    write!(&mut out_file, "{}", svg).expect("failed to write file.");

    Ok(())
}
