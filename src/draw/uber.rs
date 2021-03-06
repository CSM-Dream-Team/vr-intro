use gfx::{self, Resources, CommandBuffer, ShaderSet, Factory, Rect, Slice, Encoder};
use gfx::pso::PipelineState;
use gfx::traits::FactoryExt;
use gfx::handle::{Buffer, DepthStencilView};
use gfx::state::Rasterizer;
use gfx::format::*;

use nalgebra::{self as na, Rotation3, Vector3, Matrix4};

use super::{StyleInputs, Style, TransformBlock};
use ::mesh::{Primitive, MeshSource, Mesh, Indexing, Vert, VertNTT};
use ::{Error, ColorFormat, DepthFormat, TargetRef, DepthRef, Texture};
use ::util::NativeRepr;
use std::mem::transmute;

pub type LumMapFormat = (R32_G32_B32, Float);

/// The collection of mesh textures used by physically based rendering
#[derive(Clone)]
pub struct UberMaterial<R: Resources> {
    /// normal map
    pub normal: Texture<R, (R8_G8_B8_A8, Unorm)>,
    /// albedo map (base color)
    pub albedo: Texture<R, (R8_G8_B8_A8, Srgb)>,
    /// metalness (1=metal, 0=dielectric), roughness, flatness (0=PBR, 1=flat color) map
    pub knobs: Texture<R, (R8_G8_B8_A8, Unorm)>,
}

gfx_defines!{
    constant ParamsBlock {
        sun_matrix: [[f32; 4]; 4] = "sun_matrix",
        sun_color: [f32; 4] = "sun_color",
        sun_in_env: f32 = "sun_in_env",
        radiance_levels: i32 = "radiance_levels",

        gamma: f32 = "gamma",
        exposure: f32 = "exposure",
    }

    pipeline bg {
        verts: gfx::VertexBuffer<Vert> = (),
        transform: gfx::ConstantBuffer<TransformBlock> = "transform",
        params: gfx::ConstantBuffer<ParamsBlock> = "params",
        scissor: gfx::Scissor = (), // TODO: Replace scissoring with viewport

        color: gfx::RenderTarget<ColorFormat> = "f_color",
        depth: gfx::DepthTarget<DepthFormat> = gfx::preset::depth::PASS_TEST,

        radiance: gfx::TextureSampler<[f32; 3]> = "cube_map",
    }

    pipeline pl {
        verts: gfx::VertexBuffer<VertNTT> = (),
        transform: gfx::ConstantBuffer<TransformBlock> = "transform",
        params: gfx::ConstantBuffer<ParamsBlock> = "params",
        scissor: gfx::Scissor = (), // TODO: Replace scissoring with viewport

        color: gfx::RenderTarget<ColorFormat> = "f_color",
        depth: gfx::DepthTarget<DepthFormat> = gfx::preset::depth::LESS_EQUAL_WRITE,

        normal: gfx::TextureSampler<[f32; 4]> = "normal_tex",
        albedo: gfx::TextureSampler<[f32; 4]> = "albedo_tex",
        knobs: gfx::TextureSampler<[f32; 4]> = "knobs_tex",
        irradiance: gfx::TextureSampler<[f32; 3]> = "irradiance_map",
        radiance: gfx::TextureSampler<[f32; 3]> = "radiance_map",
        integrated_brdf: gfx::TextureSampler<[f32; 2]> = "integrated_brdf_map",

        shadow_depth: gfx::TextureSampler<f32> = "shadow_depth",
    }
}

shader!(shader {
    vertex: static_file!("shaders/transform.v.glsl")
        .define("NORM")
        .define("TEX")
        .define("TAN"),
    fragment: static_file!("shaders/uber.f.glsl")
        .define_to("I_POS", "v_pos")
        .define_to("I_NORM", "v_norm")
        .define_to("I_TEX", "v_tex")
        .define_to("I_TAN", "v_tan")
        .define_to("I_BITAN", "v_bitan")
});

shader!(bg_shader {
    vertex: static_file!("shaders/transform.v.glsl")
        .define_to("W_COORD", 1.),
    fragment: static_file!("shaders/cubebg.f.glsl")
        .define_to("I_POS", "v_pos")
});

/// The scene environment
pub struct UberEnv<R: Resources> {
    pub irradiance: Texture<R, LumMapFormat>,
    pub radiance: Texture<R, LumMapFormat>,
    pub sun_included: bool,
    pub sun_color: [f32; 4],
    pub sun_rotation: Rotation3<f32>,
    pub radiance_levels: u8,
}

/// The configuration for physically based rendering
pub struct UberInputs<R: Resources> {
    shaders: ShaderSet<R>,
    background: UberBackground<R>,
    transform: Option<TransformBlock>,
    transform_block: Buffer<R, TransformBlock>,
    env: UberEnv<R>,
    exposure: f32,
    gamma: f32,
    params_update: bool,
    params_block: Buffer<R, ParamsBlock>,
    integrated_brdf: Texture<R, (R8_G8, Unorm)>,
    shadow_depth: Texture<R, (D32, Float)>,
}

struct UberBackground<R: Resources> {
    pso: PipelineState<R, bg::Meta>,
    // shaders: ShaderSet<R>,
    mesh: Mesh<R, Vert, ()>,
}

impl<R: Resources> UberInputs<R> {
    pub fn set_env(&mut self, env: UberEnv<R>) {
        self.env = env;
        self.params_update = true;
    }

    pub fn mut_env(&mut self) -> &mut UberEnv<R> {
        self.params_update = true;
        &mut self.env
    }

    pub fn set_exposure(&mut self, exposure: f32) {
        self.exposure = exposure;
        self.params_update = true;
    }

    pub fn set_gamma(&mut self, gamma: f32) {
        self.gamma = gamma;
        self.params_update = true;
    }
}

impl<R: Resources> StyleInputs<R> for UberInputs<R> {
    fn transform(&mut self, block: TransformBlock) {
        self.transform = Some(block);
    }
    fn shader_set(&self) -> &ShaderSet<R> { &self.shaders }
}

/// Draws meshes using a physically based rendering pipeline
pub struct UberStyle<R: Resources> {
    pso: PipelineState<R, pl::Meta>,
}

fn shadow_texture<R: Resources, F: Factory<R>>(factory: &mut F)
    -> (DepthStencilView<R, (D32, Float)>, Texture<R, (D32, Float)>)
{
    use gfx::texture::*;
    use gfx::memory::{Bind, Usage};
    
    let shadow_tex = {
        let kind = Kind::D2(512, 512, AaMode::Single);
        let bind = Bind::SHADER_RESOURCE | Bind::DEPTH_STENCIL;
        let ctype = Some(gfx::format::ChannelType::Float);

        factory.create_texture(kind, 1, bind, Usage::Data, ctype).unwrap()
    };

    let resource = factory.view_texture_as_shader_resource
        ::<::ShadowDepthFormat>(
            &shadow_tex, (0, 0), gfx::format::Swizzle::new()
        ).unwrap();

    let mut sampler_info = SamplerInfo::new(
        FilterMethod::Bilinear,
        WrapMode::Clamp
    );
    sampler_info.comparison = Some(gfx::state::Comparison::LessEqual);
    let sampler = factory.create_sampler(sampler_info);

    let shadow_depth_target = factory.view_texture_as_depth_stencil(
        &shadow_tex, 0, None,
        DepthStencilFlags::empty()).unwrap();

    (shadow_depth_target, Texture {
        buffer: resource,
        sampler: sampler,
    })
}

impl<R: Resources> Style<R> for UberStyle<R> {
    type Vertex = VertNTT;
    type Inputs = UberInputs<R>;
    type Material = UberMaterial<R>;

    fn new<F: Factory<R> + FactoryExt<R>> (
        f: &mut F,
        i: &mut UberInputs<R>,
        p: Primitive,
        r: Rasterizer,
    ) -> Result<Self, Error> {
        Ok(UberStyle {
            pso: f.create_pipeline_state(&i.shaders, p, r, pl::new())?,
        })
    }

    fn init<F: Factory<R>>(
        f: &mut F,
    ) -> Result<UberInputs<R>, Error> {
        let bg_color = [0.529, 0.808, 0.980];
        let bg_bytes = unsafe {
            transmute::<[f32; 3], [u32; 3]>(bg_color)
        };
        let (_, shadow_depth) = shadow_texture(f);
        let bg_shaders = bg_shader(f)?;
        let bg_verts = vec![
            Vert { pos: [-10., -10.,  10.] },
            Vert { pos: [-10.,  10.,  10.] },
            Vert { pos: [-10., -10., -10.] },
            Vert { pos: [-10.,  10., -10.] },
            Vert { pos: [ 10., -10.,  10.] },
            Vert { pos: [ 10.,  10.,  10.] },
            Vert { pos: [ 10., -10., -10.] },
            Vert { pos: [ 10.,  10., -10.] },
        ];
        let bg_inds = vec![
            1-1, 3-1, 2-1,
            3-1, 7-1, 4-1,
            7-1, 5-1, 8-1,
            5-1, 1-1, 6-1,
            3-1, 1-1, 7-1,
            8-1, 6-1, 4-1,
            3-1, 4-1, 2-1,
            7-1, 8-1, 4-1,
            5-1, 6-1, 8-1,
            1-1, 2-1, 6-1,
            1-1, 5-1, 7-1,
            6-1, 2-1, 4-1,
        ];
        Ok(UberInputs {
            shaders: shader(f)?,
            background: UberBackground {
                pso: f.create_pipeline_state(
                    &bg_shaders,
                    Primitive::TriangleList,
                    Rasterizer::new_fill(),
                    bg::new())?,
                // shaders: bg_shaders,
                mesh: MeshSource {
                    verts: bg_verts,
                    inds: Indexing::Inds(bg_inds),
                    mat: (),
                    prim: Primitive::TriangleList,
                }.upload(f),
            },
            transform: None,
            transform_block: f.create_constant_buffer(1),
            params_update: true,
            params_block: f.create_constant_buffer(1),
            gamma: 2.2,
            exposure: 1.0,
            integrated_brdf: ::load::load_integrated_brdf(f)?,
            env: UberEnv {
                radiance: Texture::uniform_value(f, bg_bytes)?,
                irradiance: Texture::uniform_value(f, bg_bytes)?,
                sun_color: [1., 1., 1., 2.0],
                sun_rotation: Rotation3::rotation_between(
                    &Vector3::new(0., 0., -1.),
                    &Vector3::new(0., -1., 0.),
                ).expect("Could not rotate axis"),
                sun_included: false,
                radiance_levels: 1,
            },
            shadow_depth: shadow_depth,
        })
    }

    fn draw_raw<C>(
        &self,
        inputs: &mut UberInputs<R>,
        enc: &mut Encoder<R, C>,
        color: TargetRef<R>,
        depth: DepthRef<R>,
        scissor: Rect,
        slice: &Slice<R>,
        buf: Buffer<R, Self::Vertex>,
        mat: &UberMaterial<R>,
    )
        -> Result<(), Error>
        where C: CommandBuffer<R>
    {
        if let Some(t) = inputs.transform.take() {
            enc.update_constant_buffer(&inputs.transform_block, &t);
        }
        if inputs.params_update {
            let mat: Rotation3<f32> = na::convert(inputs.env.sun_rotation);
            enc.update_constant_buffer(&inputs.params_block, &ParamsBlock { 
                sun_matrix: mat.to_homogeneous().downgrade(),
                sun_color: inputs.env.sun_color,
                sun_in_env: if inputs.env.sun_included { 1. } else { 0. },
                exposure: inputs.exposure,
                gamma: inputs.gamma,
                radiance_levels: inputs.env.radiance_levels as i32,
            });
        }
        enc.draw(slice, &self.pso, &pl::Data {
            color: color,
            depth: depth,
            verts: buf,
            scissor: scissor,
            transform: inputs.transform_block.clone(),
            params: inputs.params_block.clone(),
            normal: mat.normal.clone().into_tuple(),
            albedo: mat.albedo.clone().into_tuple(),
            knobs: mat.knobs.clone().into_tuple(),
            integrated_brdf: inputs.integrated_brdf.clone().into_tuple(),
            irradiance: inputs.env.irradiance.clone().into_tuple(),
            radiance: inputs.env.radiance.clone().into_tuple(),
            shadow_depth: inputs.shadow_depth.clone().into_tuple(),
        });
        Ok(())
    }
}

impl<R: Resources> super::Painter<R, UberStyle<R>> {
    pub fn clear_env<C: CommandBuffer<R>>(
        &self,
        ctx: &mut super::DrawParams<R, C>,
    ) {
        let inputs = self.inputs.borrow();
        let bgin = &inputs.background;
        let mat: Rotation3<f32> = na::convert(inputs.env.sun_rotation);
        ctx.encoder.update_constant_buffer(&inputs.params_block, &ParamsBlock { 
            sun_matrix: mat.to_homogeneous().downgrade(),
            sun_color: inputs.env.sun_color,
            sun_in_env: if inputs.env.sun_included { 1. } else { 0. },
            exposure: inputs.exposure,
            gamma: inputs.gamma,
            radiance_levels: inputs.env.radiance_levels as i32,
        });
        for eye in &[&ctx.left, &ctx.right] {
            let trans = TransformBlock {
                eye: eye.eye.to_homogeneous().downgrade(),
                model: Matrix4::identity().downgrade(),
                view: eye.view.downgrade(),
                proj: eye.proj.downgrade(),
                clip_offset: eye.clip_offset,
            };
            ctx.encoder.update_constant_buffer(&inputs.transform_block, &trans);
            ctx.encoder.draw(&bgin.mesh.slice, &bgin.pso, &bg::Data {
                color: ctx.color.clone(),
                depth: ctx.depth.clone(),
                verts: bgin.mesh.buf.clone(),
                scissor: eye.clip,
                transform: inputs.transform_block.clone(),
                params: inputs.params_block.clone(),
                radiance: inputs.env.radiance.clone().into_tuple(),
            });
        }
    }
}
