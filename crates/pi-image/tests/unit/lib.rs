use super::*;

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = DynamicImage::new_rgb8(width, height);
    let mut buffer = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut buffer), ImageFormat::Png)
        .expect("encode png");
    buffer
}

#[test]
fn leaves_small_images_untouched() {
    let input = png(64, 48);
    let result = resize_image(&input, &ImageLimits::default()).expect("resizes");
    assert!(!result.was_resized);
    assert_eq!((result.width, result.height), (64, 48));
    assert_eq!(result.mime_type, "image/png");
    assert_eq!(
        result.data_base64,
        base64::engine::general_purpose::STANDARD.encode(&input)
    );
}

#[test]
fn scales_down_to_the_dimension_limit() {
    let input = png(4000, 2000);
    let limits = ImageLimits {
        max_width: 1000,
        max_height: 1000,
        ..ImageLimits::default()
    };
    let result = resize_image(&input, &limits).expect("resizes");
    assert!(result.was_resized);
    assert_eq!(result.width, 1000);
    assert_eq!(result.height, 500);
}

#[test]
fn honors_the_byte_limit() {
    let input = png(800, 800);
    let limits = ImageLimits {
        max_width: 2000,
        max_height: 2000,
        max_bytes: 2048,
        ..ImageLimits::default()
    };
    let result = resize_image(&input, &limits).expect("resizes under limit");
    assert!(result.data_base64.len() < 2048);
}

#[test]
fn returns_none_when_impossible() {
    let input = png(32, 32);
    let limits = ImageLimits {
        max_width: 2000,
        max_height: 2000,
        max_bytes: 8,
        ..ImageLimits::default()
    };
    // Even a tiny image cannot fit under an 8-byte base64 payload.
    assert!(resize_image(&input, &limits).is_none());
}

#[test]
fn invalid_input_is_none() {
    assert!(resize_image(b"not an image", &ImageLimits::default()).is_none());
}

#[test]
fn orientation_6_rotates_and_swaps_dimensions() {
    let image = DynamicImage::new_rgb8(40, 20);
    let rotated = apply_orientation(image, 6);
    assert_eq!((rotated.width(), rotated.height()), (20, 40));
}

#[test]
fn orientation_1_is_identity() {
    let image = DynamicImage::new_rgb8(10, 30);
    let same = apply_orientation(image, 1);
    assert_eq!((same.width(), same.height()), (10, 30));
}

#[test]
fn plain_images_have_no_orientation() {
    assert_eq!(read_orientation(&png(8, 8)), None);
}

#[test]
fn resized_output_decodes_to_the_target_dimensions() {
    let input = png(3000, 1500);
    let limits = ImageLimits {
        max_width: 1000,
        max_height: 1000,
        ..ImageLimits::default()
    };
    let result = resize_image(&input, &limits).expect("resizes");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&result.data_base64)
        .expect("valid base64");
    let decoded = image::load_from_memory(&bytes).expect("output decodes");
    assert_eq!((decoded.width(), decoded.height()), (1000, 500));
}
