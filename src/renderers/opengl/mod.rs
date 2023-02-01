pub mod gl_buffer;
pub mod shader;
pub mod shaders;
pub mod texture;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Deref;
use std::{io, mem};

use glam::{uvec2, UVec2, Vec3};
use glow::HasContext;
use image::ImageFormat;
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};
use tracing::error;

use crate::math::camera::Camera;
use crate::model::ModelTexture;
use crate::nodes::node::{InoxNode, InoxNodeUuid};
use crate::nodes::node_data::{BlendMode, Composite, InoxData, Mask, MaskMode, Part};
use crate::nodes::node_tree::InoxNodeTree;
use crate::texture::tga::read_tga;

use self::gl_buffer::{InoxGlBuffers, InoxGlBuffersBuilder};
use self::shader::ShaderCompileError;
use self::shaders::{CompositeMaskShader, CompositeShader, PartMaskShader, PartShader};
use self::texture::{Texture, TextureError};

#[derive(Debug, thiserror::Error)]
#[error("Could not initialize OpenGL renderer: {0}")]
pub enum OpenglRendererError {
    ShaderCompile(#[from] ShaderCompileError),
    Opengl(String),
}

#[derive(Default, Clone)]
pub struct GlCache {
    pub camera: Option<Camera>,
    pub viewport: Option<UVec2>,
    pub blend_mode: Option<BlendMode>,
    pub program: Option<glow::NativeProgram>,
    pub albedo: Option<usize>,
}

impl GlCache {
    pub fn update_camera(&mut self, camera: &Camera) -> bool {
        if let Some(prev_camera) = &mut self.camera {
            let mut changed = false;

            if prev_camera.position != camera.position {
                prev_camera.position = camera.position;
                changed = true;
            }
            if prev_camera.rotation != camera.rotation {
                prev_camera.rotation = camera.rotation;
                changed = true;
            }
            if prev_camera.scale != camera.scale {
                prev_camera.scale = camera.scale;
                changed = true;
            }

            changed
        } else {
            self.camera = Some(camera.clone());
            true
        }
    }

    pub fn update_viewport(&mut self, viewport: UVec2) -> bool {
        if let Some(prev_viewport) = self.viewport.replace(viewport) {
            prev_viewport != viewport
        } else {
            true
        }
    }

    pub fn update_blend_mode(&mut self, blend_mode: BlendMode) -> bool {
        if let Some(prev_mode) = self.blend_mode.replace(blend_mode) {
            prev_mode != blend_mode
        } else {
            true
        }
    }

    pub fn update_program(&mut self, program: glow::NativeProgram) -> bool {
        if let Some(prev_program) = self.program.replace(program) {
            prev_program != program
        } else {
            true
        }
    }

    pub fn update_albedo(&mut self, albedo: usize) -> bool {
        if let Some(prev_texture) = self.albedo.replace(albedo) {
            prev_texture != albedo
        } else {
            true
        }
    }
}

#[derive(Debug)]
enum NodeDrawInfo {
    Part { index_offset: u16 },
    Composite { children: Vec<InoxNodeUuid> },
}

pub struct OpenglRenderer<T = ()> {
    gl: glow::Context,
    pub camera: Camera,
    pub viewport: UVec2,
    cache: RefCell<GlCache>,
    is_compositing: Cell<bool>,

    part_bufs: InoxGlBuffers,
    composite_bufs: InoxGlBuffers,

    composite_framebuffer: glow::NativeFramebuffer,
    cf_albedo: glow::NativeTexture,
    cf_emissive: glow::NativeTexture,
    cf_bump: glow::NativeTexture,
    cf_stencil: glow::NativeTexture,

    part_shader: PartShader,
    part_mask_shader: PartMaskShader,
    composite_shader: CompositeShader,
    composite_mask_shader: CompositeMaskShader,

    textures: Vec<Texture>,

    pub nodes: InoxNodeTree<T>,
    nodes_zsorted: Vec<InoxNodeUuid>,
    nodes_draw_info: HashMap<InoxNodeUuid, NodeDrawInfo>,
}

impl<T> OpenglRenderer<T> {
    pub fn new(
        gl: glow::Context,
        viewport: UVec2,
        nodes: InoxNodeTree<T>,
    ) -> Result<Self, OpenglRendererError> {
        // Composite vertices and UVs are initialized with data for composite rendering
        let mut composite_bufs = InoxGlBuffersBuilder::with_quad();
        let mut part_bufs = InoxGlBuffersBuilder::new();

        let nodes_zsorted = nodes.zsorted_root();
        let mut nodes_draw_info = HashMap::new();
        for &uuid in &nodes_zsorted {
            let node = nodes.get_node(uuid).unwrap();

            match node.data {
                InoxData::Part(ref part) => {
                    let index_offset = part_bufs.push(&part.mesh);
                    nodes_draw_info.insert(uuid, NodeDrawInfo::Part { index_offset });
                }
                InoxData::Composite(_) => {
                    // Children include the parent composite, so we have to filter it out.
                    // TODO: wait... does it make sense for it to do that?
                    let children = nodes
                        .zsorted_child(node.uuid)
                        .into_iter()
                        .filter(|uuid| *uuid != node.uuid)
                        .collect::<Vec<_>>();

                    // put composite children's meshes into composite bufs
                    for &uuid in &children {
                        let node = nodes.get_node(uuid).unwrap();

                        if let InoxData::Part(ref part) = node.data {
                            let index_offset = composite_bufs.push(&part.mesh);
                            nodes_draw_info.insert(uuid, NodeDrawInfo::Part { index_offset });
                        }
                    }

                    nodes_draw_info.insert(uuid, NodeDrawInfo::Composite { children });
                }
                _ => (),
            }
        }

        // Initialize buffers
        let part_bufs = unsafe { part_bufs.upload(&gl)? };
        let composite_bufs = unsafe { composite_bufs.upload(&gl)? };

        // Initialize framebuffers
        let composite_framebuffer;
        let cf_albedo;
        let cf_emissive;
        let cf_bump;
        let cf_stencil;
        unsafe {
            cf_albedo = gl.create_texture().map_err(OpenglRendererError::Opengl)?;
            cf_emissive = gl.create_texture().map_err(OpenglRendererError::Opengl)?;
            cf_bump = gl.create_texture().map_err(OpenglRendererError::Opengl)?;
            cf_stencil = gl.create_texture().map_err(OpenglRendererError::Opengl)?;

            composite_framebuffer = gl
                .create_framebuffer()
                .map_err(OpenglRendererError::Opengl)?;
        }

        // Shaders
        let part_shader = PartShader::new(&gl)?;
        let part_mask_shader = PartMaskShader::new(&gl)?;
        let composite_shader = CompositeShader::new(&gl)?;
        let composite_mask_shader = CompositeMaskShader::new(&gl)?;

        let mut renderer = Self {
            gl,
            camera: Camera::default(),
            viewport,
            cache: RefCell::new(GlCache::default()),
            is_compositing: Cell::new(false),

            part_bufs,
            composite_bufs,

            composite_framebuffer,
            cf_albedo,
            cf_emissive,
            cf_bump,
            cf_stencil,

            part_shader,
            part_mask_shader,
            composite_shader,
            composite_mask_shader,

            textures: Vec::new(),

            nodes,
            nodes_zsorted,
            nodes_draw_info,
        };

        renderer.resize(viewport.x, viewport.y);
        unsafe { renderer.attach_framebuffer_textures() };

        Ok(renderer)
    }

    pub fn upload_model_textures(
        &mut self,
        model_textures: &[ModelTexture],
    ) -> Result<(), TextureError> {
        // decode textures in parallel
        let images = model_textures
            .par_iter()
            .filter_map(|mtex| {
                if mtex.format == ImageFormat::Tga {
                    match read_tga(&mut io::Cursor::new(&mtex.data)) {
                        Ok(img) => Some((
                            img.data,
                            img.header.width() as u32,
                            img.header.height() as u32,
                        )),
                        Err(e) => {
                            error!("{}", e);
                            None
                        }
                    }
                } else {
                    let img_buf = image::load_from_memory_with_format(&mtex.data, mtex.format)
                        .map_err(TextureError::LoadData);

                    match img_buf {
                        Ok(img_buf) => {
                            let img_buf = img_buf.into_rgba8();
                            Some((img_buf.to_vec(), img_buf.width(), img_buf.height()))
                        }
                        Err(e) => {
                            error!("{}", e);
                            None
                        }
                    }
                }
            })
            .collect::<Vec<_>>();

        // upload textures
        for (pixels, width, height) in images {
            let tex = texture::Texture::from_raw_pixels(&self.gl, &pixels, width, height)?;
            self.textures.push(tex);
        }

        Ok(())
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.viewport = uvec2(w, h);

        let gl = &self.gl;
        unsafe {
            gl.viewport(0, 0, w as i32, h as i32);

            // Reupload composite framebuffer textures
            texture::upload_empty(gl, self.cf_albedo, w, h, glow::UNSIGNED_BYTE);
            texture::upload_empty(gl, self.cf_emissive, w, h, glow::FLOAT);
            texture::upload_empty(gl, self.cf_bump, w, h, glow::UNSIGNED_BYTE);

            gl.bind_texture(glow::TEXTURE_2D, Some(self.cf_stencil));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::DEPTH24_STENCIL8 as i32,
                w as i32,
                h as i32,
                0,
                glow::DEPTH_STENCIL,
                glow::UNSIGNED_INT_24_8,
                None,
            );

            self.attach_framebuffer_textures();
        }

        self.update_camera();
    }

    pub fn clear(&self) {
        unsafe { self.gl.clear(glow::COLOR_BUFFER_BIT) };
    }

    #[inline]
    fn bind_shader<S: Deref<Target = glow::NativeProgram>>(&self, shader: &S) {
        let program = **shader;
        unsafe { self.gl.use_program(Some(program)) };
    }

    /// Pushes an OpenGL debug group.
    /// This is very useful to debug OpenGL calls per node with `apitrace`, as it will nest calls inside of labels,
    /// making it trivial to know which calls correspond to which nodes.
    ///
    /// It is a no-op on platforms that don't support it (only MacOS so far).
    #[inline]
    fn push_debug_group(&self, name: &str) {
        #[cfg(not(target_os = "macos"))]
        unsafe {
            self.gl
                .push_debug_group(glow::DEBUG_SOURCE_APPLICATION, 0, name);
        }
    }

    /// Pops the last OpenGL debug group.
    ///
    /// It is a no-op on platforms that don't support it (only MacOS so far).
    #[inline]
    fn pop_debug_group(&self) {
        #[cfg(not(target_os = "macos"))]
        unsafe {
            self.gl.pop_debug_group();
        }
    }

    /// Updates the camera in the GL cache and returns whether it changed.
    fn update_camera(&self) -> bool {
        {
            let mut cache = self.cache.borrow_mut();
            if !cache.update_camera(&self.camera) && !cache.update_viewport(self.viewport) {
                return false;
            }
        }

        let matrix = self.camera.matrix(self.viewport.as_vec2());

        self.bind_shader(&self.part_mask_shader);
        self.part_mask_shader.set_mvp(&self.gl, matrix);

        self.bind_shader(&self.part_shader);
        self.part_shader.set_mvp(&self.gl, matrix);

        self.bind_shader(&self.composite_shader);
        self.composite_shader.set_mvp(&self.gl, matrix);

        self.bind_shader(&self.composite_mask_shader);
        self.composite_mask_shader.set_mvp(&self.gl, matrix);

        true
    }

    /// Set blending mode. See `BlendMode` for supported blend modes.
    pub fn set_blend_mode(&self, blend_mode: BlendMode) {
        if !self.cache.borrow_mut().update_blend_mode(blend_mode) {
            return;
        }

        let gl = &self.gl;
        unsafe {
            match blend_mode {
                BlendMode::Normal => {
                    gl.blend_equation(glow::FUNC_ADD);
                    gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                }
                BlendMode::Multiply => {
                    gl.blend_equation(glow::FUNC_ADD);
                    gl.blend_func(glow::DST_COLOR, glow::ONE_MINUS_SRC_ALPHA);
                }
                BlendMode::ColorDodge => {
                    gl.blend_equation(glow::FUNC_ADD);
                    gl.blend_func(glow::DST_COLOR, glow::ONE);
                }
                BlendMode::LinearDodge => {
                    gl.blend_equation(glow::FUNC_ADD);
                    gl.blend_func(glow::ONE, glow::ONE);
                }
                BlendMode::Screen => {
                    gl.blend_equation(glow::FUNC_ADD);
                    gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_COLOR);
                }
                BlendMode::ClipToLower => {
                    gl.blend_equation(glow::FUNC_ADD);
                    gl.blend_func(glow::DST_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                }
                BlendMode::SliceFromLower => {
                    gl.blend_equation(glow::FUNC_SUBTRACT);
                    gl.blend_func(glow::ONE_MINUS_DST_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                }
            }
        }
    }

    pub fn draw_model(&self) {
        self.update_camera();
        unsafe { self.gl.enable(glow::BLEND) };

        for uuid in &self.nodes_zsorted {
            if let Some(ntr) = self.nodes_draw_info.get(uuid) {
                self.draw_node(*uuid, ntr, false, false);
            }
        }
    }

    #[inline]
    fn bind_part_textures(&self, part: &Part) {
        if !self.cache.borrow_mut().update_albedo(part.tex_albedo) {
            return;
        }

        let gl = &self.gl;
        self.textures[part.tex_albedo].bind_on(gl, 0);
        self.textures[part.tex_bumpmap].bind_on(gl, 1);
        self.textures[part.tex_emissive].bind_on(gl, 2);
    }

    fn draw_node(
        &self,
        uuid: InoxNodeUuid,
        ndi: &NodeDrawInfo,
        is_composite_child: bool,
        is_mask: bool,
    ) {
        match ndi {
            NodeDrawInfo::Part { index_offset } => {
                let node = self.nodes.get_node(uuid).unwrap();
                if let InoxData::Part(ref part) = node.data {
                    self.draw_part(node, part, *index_offset, is_composite_child, is_mask);
                }
            }
            NodeDrawInfo::Composite { children } => {
                let node = self.nodes.get_node(uuid).unwrap();
                if let InoxData::Composite(ref composite) = node.data {
                    self.draw_composite(node, composite, children);
                }
            }
        }
    }

    unsafe fn attach_framebuffer_textures(&self) {
        let gl = &self.gl;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.composite_framebuffer));

        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(self.cf_albedo),
            0,
        );
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT1,
            glow::TEXTURE_2D,
            Some(self.cf_emissive),
            0,
        );
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT2,
            glow::TEXTURE_2D,
            Some(self.cf_bump),
            0,
        );
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::DEPTH_STENCIL_ATTACHMENT,
            glow::TEXTURE_2D,
            Some(self.cf_stencil),
            0,
        );

        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
    }

    ////////////////////////
    //// Part rendering ////
    ////////////////////////

    fn draw_part_mask(&self, mask: &Mask, is_composite_child: bool) {
        let gl = &self.gl;

        // begin draw mask
        unsafe {
            // Enable writing to stencil buffer and disable writing to color buffer
            gl.color_mask(false, false, false, false);
            gl.stencil_op(glow::KEEP, glow::KEEP, glow::REPLACE);
            gl.stencil_func(glow::ALWAYS, (mask.mode == MaskMode::Mask) as i32, 0xff);
            gl.stencil_mask(0xff);
        }

        // draw mask
        let ndi = &self.nodes_draw_info[&mask.source];
        self.draw_node(mask.source, ndi, is_composite_child, true);

        // end draw mask
        unsafe {
            gl.color_mask(true, true, true, true);
        }
    }

    fn draw_part(
        &self,
        node: &InoxNode<T>,
        part: &Part,
        index_offset: u16,
        is_composite_child: bool,
        is_mask: bool,
    ) {
        self.push_debug_group(&node.name);

        let gl = &self.gl;
        let masks = &part.draw_state.masks;

        if !masks.is_empty() {
            self.push_debug_group("Masks");

            // begin mask
            unsafe {
                // Enable and clear the stencil buffer so we can write our mask to it
                gl.enable(glow::STENCIL_TEST);
                gl.clear_stencil(!part.draw_state.has_masks() as i32);
                gl.clear(glow::STENCIL_BUFFER_BIT);
            }

            for mask in &part.draw_state.masks {
                self.draw_part_mask(mask, is_composite_child);
            }

            self.pop_debug_group();

            // begin mask content
            unsafe {
                gl.stencil_func(glow::EQUAL, 1, 0xff);
                gl.stencil_mask(0x00);
            }
        }

        // Position of current node by adding up its ancestors' positions
        let offset = self
            .nodes
            .ancestors(node.uuid)
            .filter_map(|ancestor| self.nodes.arena.get(ancestor))
            .map(|node| node.get().transform.translation)
            .sum::<Vec3>()
            .truncate();

        self.bind_part_textures(part);
        self.set_blend_mode(part.draw_state.blend_mode);

        if is_mask {
            let part_mask_shader = &self.part_mask_shader;
            self.bind_shader(part_mask_shader);

            // vert uniforms
            part_mask_shader.set_offset(gl, offset);

            // frag uniforms
            part_mask_shader.set_threshold(gl, part.draw_state.mask_threshold.clamp(0.0, 1.0));
        } else {
            let part_shader = &self.part_shader;
            self.bind_shader(part_shader);

            // vert uniforms
            part_shader.set_offset(gl, offset);

            // frag uniforms
            part_shader.set_opacity(gl, part.draw_state.opacity);
            part_shader.set_mult_color(gl, part.draw_state.tint);
            part_shader.set_screen_color(gl, part.draw_state.screen_tint);
        }

        if is_composite_child {
            self.composite_bufs.bind(gl);
        } else {
            self.part_bufs.bind(gl);
        }

        unsafe {
            gl.draw_elements(
                glow::TRIANGLES,
                part.mesh.indices.len() as i32,
                glow::UNSIGNED_SHORT,
                index_offset as i32 * mem::size_of::<u16>() as i32,
            );
        }

        if !masks.is_empty() {
            // end mask
            unsafe {
                // We're done stencil testing, disable it again so that we don't accidentally mask more stuff out
                gl.stencil_mask(0xff);
                gl.stencil_func(glow::ALWAYS, 1, 0xff);
                gl.disable(glow::STENCIL_TEST);
            }
        }

        self.pop_debug_group();
    }

    /////////////////////////////
    //// Composite rendering ////
    /////////////////////////////

    /// Begin a composition step
    fn begin_composite(&self) {
        if self.is_compositing.get() {
            // We don't allow recursive compositing
            return;
        }
        self.is_compositing.set(true);

        let gl = &self.gl;
        unsafe {
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.composite_framebuffer));
            gl.draw_buffers(&[
                glow::COLOR_ATTACHMENT0,
                glow::COLOR_ATTACHMENT1,
                glow::COLOR_ATTACHMENT2,
            ]);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);

            // Everything else is the actual texture used by the meshes at id 0
            gl.active_texture(glow::TEXTURE0);
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
        }
    }

    /// End a composition step, re-binding the internal framebuffer
    fn end_composite(&self) {
        if !self.is_compositing.get() {
            // We don't allow recursive compositing
            return;
        }
        self.is_compositing.set(false);

        let gl = &self.gl;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.draw_buffers(&[
                glow::COLOR_ATTACHMENT0,
                glow::COLOR_ATTACHMENT1,
                glow::COLOR_ATTACHMENT2,
            ]);
            gl.flush();
        }
    }

    fn draw_composite(&self, node: &InoxNode<T>, composite: &Composite, children: &[InoxNodeUuid]) {
        if children.is_empty() {
            // Optimization: Nothing to be drawn, skip context switching
            return;
        }

        self.push_debug_group(&node.name);

        self.begin_composite();
        for uuid in children {
            if *uuid == node.uuid {
                // just in case it slips itself in its own children... (r/outofcontext)
                continue;
            }
            if let Some(ndi) = self.nodes_draw_info.get(uuid) {
                self.draw_node(*uuid, ndi, true, false);
            }
        }
        self.end_composite();

        let gl = &self.gl;
        unsafe {
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.cf_albedo));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.cf_emissive));
            gl.active_texture(glow::TEXTURE2);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.cf_bump));
        }

        let comp = &composite.draw_state;
        self.set_blend_mode(comp.blend_mode);

        let opacity = comp.opacity.clamp(0.0, 1.0);
        let tint = comp.tint.clamp(Vec3::ZERO, Vec3::ONE);
        let screen_tint = comp.screen_tint.clamp(Vec3::ZERO, Vec3::ONE);

        self.bind_shader(&self.composite_shader);
        self.composite_shader.set_opacity(gl, opacity);
        self.composite_shader.set_mult_color(gl, tint);
        self.composite_shader.set_screen_color(gl, screen_tint);

        self.composite_bufs.bind(gl);
        unsafe {
            gl.draw_elements(glow::TRIANGLES, 6, glow::UNSIGNED_SHORT, 0);
        }

        self.pop_debug_group();
    }
}
