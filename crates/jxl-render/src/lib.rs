use std::{io::Read, collections::BTreeMap};

use jxl_bitstream::{Bitstream, Bundle, header::Headers};
use jxl_frame::{Frame, header::{FrameType, Encoding}};
use jxl_grid::Grid;

mod buffer;
mod color;
mod dct;
mod error;
mod vardct;
pub use buffer::FrameBuffer;
pub use error::{Error, Result};

#[derive(Debug)]
pub struct RenderContext<'a> {
    image_header: &'a Headers,
    frames: Vec<Frame<'a>>,
    lf_frame: Vec<usize>,
    reference: Vec<usize>,
}

impl<'a> RenderContext<'a> {
    pub fn new(image_header: &'a Headers) -> Self {
        Self {
            image_header,
            frames: Vec::new(),
            lf_frame: vec![usize::MAX; 4],
            reference: vec![usize::MAX; 4],
        }
    }

    fn metadata(&self) -> &'a jxl_bitstream::header::ImageMetadata {
        &self.image_header.metadata
    }

    fn xyb_encoded(&self) -> bool {
        self.image_header.metadata.xyb_encoded
    }

    fn preserve_frame(&mut self, frame: Frame<'a>) {
        let header = frame.header();
        let idx = self.frames.len();
        let is_last = header.is_last;

        if !is_last && (header.duration == 0 || header.save_as_reference != 0) && header.frame_type != FrameType::LfFrame {
            let ref_idx = header.save_as_reference as usize;
            self.reference[ref_idx] = idx;
        }
        if header.lf_level != 0 {
            let lf_idx = header.lf_level as usize - 1;
            self.lf_frame[lf_idx] = idx;
        }
        self.frames.push(frame);
    }
}

impl RenderContext<'_> {
    pub fn width(&self) -> u32 {
        self.image_header.size.width
    }

    pub fn height(&self) -> u32 {
        self.image_header.size.height
    }
}

impl RenderContext<'_> {
    #[cfg(feature = "mt")]
    pub fn load_cropped<R: Read + Send>(&mut self, bitstream: &mut Bitstream<R>, region: Option<(u32, u32, u32, u32)>) -> Result<()> {
        let image_header = self.image_header;

        loop {
            bitstream.zero_pad_to_byte()?;
            let mut frame = Frame::parse(bitstream, image_header)?;
            let header = frame.header();
            let is_last = header.is_last;
            eprintln!("Decoding {}x{} frame (type={:?}, upsampling={}, lf_level={})", header.width, header.height, header.frame_type, header.upsampling, header.lf_level);

            if header.frame_type.is_normal_frame() {
                frame.load_cropped_par(bitstream, region)?;
            } else {
                frame.load_all_par(bitstream)?;
            }
            frame.complete()?;

            let toc = frame.toc();
            let bookmark = toc.bookmark() + (toc.total_byte_size() * 8);
            self.preserve_frame(frame);
            if is_last {
                break;
            }

            bitstream.skip_to_bookmark(bookmark)?;
        }
        Ok(())
    }

    #[cfg(not(feature = "mt"))]
    pub fn load_cropped<R: Read>(&mut self, bitstream: &mut Bitstream<R>, region: Option<(u32, u32, u32, u32)>) -> Result<()> {
        let image_header = self.image_header;

        loop {
            bitstream.zero_pad_to_byte()?;
            let mut frame = Frame::parse(bitstream, image_header)?;
            let header = frame.header();
            let is_last = header.is_last;
            eprintln!("Decoding {}x{} frame (type={:?}, upsampling={}, lf_level={})", header.width, header.height, header.frame_type, header.upsampling, header.lf_level);

            if header.frame_type.is_normal_frame() {
                frame.load_cropped(bitstream, region)?;
            } else {
                frame.load_all(bitstream)?;
            }
            frame.complete()?;

            let toc = frame.toc();
            let bookmark = toc.bookmark() + (toc.total_byte_size() * 8);
            self.preserve_frame(frame);
            if is_last {
                break;
            }

            bitstream.skip_to_bookmark(bookmark)?;
        }
        Ok(())
    }

    #[cfg(feature = "mt")]
    pub fn load_all_frames<R: Read + Send>(&mut self, bitstream: &mut Bitstream<R>) -> Result<()> {
        self.load_cropped(bitstream, None)
    }

    #[cfg(not(feature = "mt"))]
    pub fn load_all_frames<R: Read>(&mut self, bitstream: &mut Bitstream<R>) -> Result<()> {
        self.load_cropped(bitstream, None)
    }

    pub fn tmp_rgba_be_interleaved<F>(&self, f: F) -> Result<()>
    where
        F: FnMut(&[u8]) -> jxl_frame::Result<()>,
    {
        let frame = self.frames.last().unwrap();
        frame.rgba_be_interleaved(f)?;
        Ok(())
    }
}

impl RenderContext<'_> {
    pub fn render(&self) -> Result<FrameBuffer> {
        let Some(target_frame) = self.frames
            .iter()
            .find(|f| f.header().frame_type.is_normal_frame())
            else {
                panic!("No regular frame found");
            };

        self.render_frame(target_frame)
    }
}

impl<'f> RenderContext<'f> {
    fn render_frame<'a>(&'a self, frame: &'a Frame<'f>) -> Result<FrameBuffer> {
        match frame.header().encoding {
            Encoding::Modular => {
                todo!()
            },
            Encoding::VarDct => {
                self.render_vardct(frame)
            },
        }
    }

    fn render_vardct<'a>(&'a self, frame: &'a Frame<'f>) -> Result<FrameBuffer> {
        let header = self.image_header;
        let frame_header = frame.header();
        let frame_data = frame.data();
        let lf_global = frame_data.lf_global.as_ref().ok_or(Error::IncompleteFrame)?;
        let lf_global_vardct = lf_global.vardct.as_ref().unwrap();
        let hf_global = frame_data.hf_global.as_ref().ok_or(Error::IncompleteFrame)?;
        let hf_global = hf_global.as_ref().expect("HfGlobal not found for VarDCT frame");
        let subsampled = frame_header.jpeg_upsampling.into_iter().any(|x| x != 0);

        // Modular extra channels are already merged into GlobalModular,
        // so it's okay to drop PassGroup modular
        let mut group_coeffs: BTreeMap<_, jxl_frame::data::HfCoeff> = BTreeMap::new();
        for (&(_, group_idx), group_pass) in &frame_data.group_pass {
            let hf_coeff = group_pass.hf_coeff.as_ref().unwrap();
            group_coeffs
                .entry(group_idx)
                .and_modify(|data| {
                    data.merge(hf_coeff);
                })
                .or_insert_with(|| hf_coeff.clone());
        }

        let quantizer = &lf_global_vardct.quantizer;
        let oim = &header.metadata.opsin_inverse_matrix;
        let dequant_matrices = &hf_global.dequant_matrices;
        let lf_chan_corr = &lf_global_vardct.lf_chan_corr;

        if frame_header.flags.use_lf_frame() {
            todo!();
        }

        let dequantized_lf = frame_data.lf_group
            .iter()
            .map(|(&lf_group_idx, data)| {
                let lf_coeff = data.lf_coeff.as_ref().unwrap();
                let hf_meta = data.hf_meta.as_ref().unwrap();
                let mut dequant_lf = vardct::dequant_lf(frame_header, &lf_global.lf_dequant, quantizer, lf_coeff);
                if !subsampled {
                    vardct::chroma_from_luma_lf(&mut dequant_lf, &lf_global_vardct.lf_chan_corr);
                }
                (lf_group_idx, (dequant_lf, hf_meta))
            })
            .collect::<BTreeMap<_, _>>();

        let group_dim = frame_header.group_dim();
        let groups_per_row = frame_header.groups_per_row();

        // key is (y, x) to match raster order
        let dequantized_coeff = group_coeffs
            .into_iter()
            .flat_map(|(group_idx, hf_coeff)| {
                let lf_group_id = frame_header.lf_group_idx_from_group_idx(group_idx);
                let (lf_dequant, hf_meta) = dequantized_lf.get(&lf_group_id).unwrap();
                let x_from_y = &hf_meta.x_from_y;
                let b_from_y = &hf_meta.b_from_y;

                let group_row = group_idx / groups_per_row;
                let group_col = group_idx % groups_per_row;
                let base_lf_left = (group_col % 8) * group_dim;
                let base_lf_top = (group_row % 8) * group_dim;

                hf_coeff.data
                    .into_iter()
                    .map(move |((bx, by), coeff_data)| {
                        let dct_select = coeff_data.dct_select;
                        let y = vardct::dequant_hf_varblock(&coeff_data, 1, oim, quantizer, dequant_matrices, None);
                        let x = vardct::dequant_hf_varblock(&coeff_data, 0, oim, quantizer, dequant_matrices, Some(frame_header.x_qm_scale));
                        let b = vardct::dequant_hf_varblock(&coeff_data, 2, oim, quantizer, dequant_matrices, Some(frame_header.b_qm_scale));
                        let mut yxb = [y, x, b];

                        let bx = bx * 8;
                        let by = by * 8;
                        let lf_left = base_lf_left + bx;
                        let lf_top = base_lf_top + by;
                        if !subsampled {
                            vardct::chroma_from_luma_hf(&mut yxb, lf_left, lf_top, x_from_y, b_from_y, lf_chan_corr);
                        }

                        let (bw, bh) = coeff_data.dct_select.dct_select_size();
                        for (coeff, lf_dequant) in yxb.iter_mut().zip(lf_dequant.iter()) {
                            let lf_subgrid = lf_dequant.subgrid((lf_left / 8) as i32, (lf_top / 8) as i32, bw, bh);
                            let llf = vardct::llf_from_lf(lf_subgrid, dct_select);
                            coeff.insert_subgrid(llf, 0, 0);
                        }
                        ((group_col * group_dim + by, group_row * group_dim + bx), (dct_select, yxb))
                    })
            })
            .collect::<BTreeMap<_, _>>();

        todo!("coeffs to samples")
    }
}
