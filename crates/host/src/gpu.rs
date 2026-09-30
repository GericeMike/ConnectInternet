//! T3a：GPU 侧 BGRA→NV12 转换（D3D11 像素着色器）+ NV12 回读。
//!
//! 背景（docs/M2-进展.md 分段计时）：enc p50 17ms 里 swscale BGRA→NV12 占 10ms，
//! 是 CPU 单线程软件转换。本模块把转换搬到 WGC 同一个 D3D11 设备上：
//! 两次全屏绘制（Y → R8 全分辨率、UV → R8G8 半分辨率，BT.601 limited 系数对齐
//! swscale 旧路径）→ CopySubresourceRegion 按平面拷入 NV12 纹理（R8→Y 平面、
//! R8G8→UV 平面是 D3D11 文档支持的平面兼容拷贝）→ staging 回读 3MB。
//!
//! 为什么不用 VideoProcessor：VP 输出视图要求 NV12 纹理带 RENDER_TARGET 绑定，
//! 而视频格式 RT 绑定要求设备以 D3D11_CREATE_DEVICE_VIDEO_SUPPORT 创建——
//! windows-capture 内部设备只带 BGRA_SUPPORT（实测 E_INVALIDARG）。着色器方案
//! 全部走公开 API + 普通格式 RT，无设备 flag 要求。
//!
//! 全部调用发生在捕获线程（WGC 设备所在线程），无跨线程设备访问。
//! 初始化失败时 serve.rs 回退 BGRA 直读 + swscale 旧路径，不影响可用性。

use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_SAMPLE_DESC,
};

/// 回读结果：连续缓冲，Y 平面（h 行 × w 字节）后接 UV 交错平面（h/2 行 × w 字节），
/// 两平面同一行距（DXGI NV12 布局约定）
pub struct Nv12View<'a> {
    pub buf: &'a [u8],
    pub pitch: usize,
}

const HLSL: &str = r#"
Texture2D<float4> g_src : register(t0);
SamplerState g_smp : register(s0);

struct VsOut {
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
};

// 全屏三角形（SV_VertexID，无需顶点缓冲）；D3D NDC y=+1 是顶部，纹理 v=0 也是顶行
VsOut vs_main(uint id : SV_VertexID)
{
    VsOut o;
    float2 xy = float2(id == 1 ? 3.0 : -1.0, id == 2 ? 3.0 : -1.0);
    o.pos = float4(xy, 0.0, 1.0);
    o.uv = float2((xy.x + 1.0) * 0.5, 1.0 - (xy.y + 1.0) * 0.5);
    return o;
}

// BT.601 limited（对齐 swscale BGRZ→NV12 默认系数；BGRA SRV 的 Sample 分量已按 rgba 摆好）
static const float3 KY = float3(0.2567880645, 0.5041299466, 0.0979057695);
static const float3 KU = float3(-0.1482228180, -0.2909927983, 0.4392156863);
static const float3 KV = float3(0.4392156863, -0.3677882963, -0.0714273905);

float ps_y(VsOut i) : SV_Target
{
    float3 rgb = g_src.Sample(g_smp, i.uv).rgb;
    return dot(rgb, KY) + 0.0627451018; // + 16/255
}

float2 ps_uv(VsOut i) : SV_Target
{
    float3 rgb = g_src.Sample(g_smp, i.uv).rgb;
    // 半分辨率视口 + 双线性采样 ≈ 2x2 邻域均值（4:2:0 色度下采样）
    return float2(dot(rgb, KU) + 0.5, dot(rgb, KV) + 0.5);
}
"#;

fn compile_shader(entry: &[u8], target: &[u8]) -> windows::core::Result<ID3DBlob> {
    let mut blob: Option<ID3DBlob> = None;
    let mut err: Option<ID3DBlob> = None;
    let r = unsafe {
        D3DCompile(
            HLSL.as_ptr().cast(),
            HLSL.len(),
            windows::core::PCSTR(b"gpu_convert.hlsl\0".as_ptr()),
            None,
            None,
            windows::core::PCSTR(entry.as_ptr()),
            windows::core::PCSTR(target.as_ptr()),
            0,
            0,
            &mut blob,
            Some(&mut err),
        )
    };
    if let Err(e) = &r {
        if let Some(eblob) = &err {
            let msg = unsafe {
                std::slice::from_raw_parts(eblob.GetBufferPointer() as *const u8, eblob.GetBufferSize())
            };
            eprintln!("[gpu] shader 编译失败({e}): {}", String::from_utf8_lossy(msg));
        }
        return Err(r.unwrap_err());
    }
    blob.ok_or(windows::core::Error::from_hresult(windows::core::HRESULT(-1)))
}

fn bytecode(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
}

fn viewport(w: u32, h: u32) -> D3D11_VIEWPORT {
    D3D11_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: w as f32,
        Height: h as f32,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }
}

pub struct GpuConverter {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    rs: ID3D11RasterizerState,
    /// 每帧源 BGRA 的私有拷贝（WGC 帧纹理不一定带 SHADER_RESOURCE 绑定，先 CopyResource 一次，
    /// GPU 内拷贝 ~0.1ms，换 SRV 可建）
    bgra_tex: ID3D11Texture2D,
    bgra_srv: ID3D11ShaderResourceView,
    y_tex: ID3D11Texture2D,
    y_rtv: ID3D11RenderTargetView,
    uv_tex: ID3D11Texture2D,
    uv_rtv: ID3D11RenderTargetView,
    /// 双平面 staging 回读（M2.5a 修订）：不再经 NV12 纹理平面拷贝——
    /// CopySubresourceRegion R8→NV12 平面在部分 Intel 驱动上会静默失败
    /// （被控端 UHD 620 全绿屏实锤；4060 正常）。R8G8 的行布局恰好等于
    /// NV12 的 UV 交错行，逐行 memcpy 拼 NV12 即可。
    staging_y: ID3D11Texture2D,
    staging_uv: ID3D11Texture2D,
    w: u32,
    h: u32,
    /// 回读暂存（pitch 对齐后 Y+UV 全量），复用避免每帧分配
    scratch: Vec<u8>,
    pitch: usize,
    /// WGC 帧纹理可直接建 SRV（省掉每帧 8MB 中间拷贝；不支持则走 CopyResource 中转）
    frame_srv_ok: bool,
    /// 探活计数（每 90 帧输出一次非零占比，诊断用）
    probe_n: u32,
}

impl GpuConverter {
    /// 在给定设备上建转换器（WGC 捕获线程调用，device 来自帧纹理 GetDevice）
    pub fn new(device: &ID3D11Device, w: u32, h: u32) -> windows::core::Result<Self> {
        let ctx = unsafe { device.GetImmediateContext() }?;

        let vs_blob = compile_shader(b"vs_main\0", b"vs_4_0\0")?;
        let ps_y_blob = compile_shader(b"ps_y\0", b"ps_4_0\0")?;
        let ps_uv_blob = compile_shader(b"ps_uv\0", b"ps_4_0\0")?;
        let mut out = None;
        unsafe { device.CreateVertexShader(bytecode(&vs_blob), None, Some(&mut out as *mut _))? };
        let vs = out.unwrap();
        let mut out = None;
        unsafe { device.CreatePixelShader(bytecode(&ps_y_blob), None, Some(&mut out as *mut _))? };
        let ps_y = out.unwrap();
        let mut out = None;
        unsafe { device.CreatePixelShader(bytecode(&ps_uv_blob), None, Some(&mut out as *mut _))? };
        let ps_uv = out.unwrap();

        // 双线性 + clamp（Y 是 1:1，UV 半分辨率下 ≈ 2x2 均值）
        let sdesc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ..Default::default()
        };
        let mut out = None;
        unsafe { device.CreateSamplerState(&sdesc, Some(&mut out as *mut _))? };
        let sampler = out.unwrap();

        // 默认光栅化状态剔除背面（CCW）；全屏三角形按 CCW 写最省心，直接关剔除
        let rdesc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            ..Default::default()
        };
        let mut out = None;
        unsafe { device.CreateRasterizerState(&rdesc, Some(&mut out as *mut _))? };
        let rs = out.unwrap();

        let make_tex = |fmt, tw, th, bind: u32| -> windows::core::Result<ID3D11Texture2D> {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: tw,
                Height: th,
                MipLevels: 1,
                ArraySize: 1,
                Format: fmt,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, // Count=0 会 E_INVALIDARG
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: bind,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut t: Option<ID3D11Texture2D> = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut t as *mut _))? };
            t.ok_or(windows::core::Error::from_hresult(windows::core::HRESULT(-1)))
        };

        let bgra_tex = make_tex(DXGI_FORMAT_B8G8R8A8_UNORM, w, h, D3D11_BIND_SHADER_RESOURCE.0 as u32)?;
        let mut out = None;
        unsafe { device.CreateShaderResourceView(&bgra_tex, None, Some(&mut out as *mut _))? };
        let bgra_srv = out.unwrap();

        let y_tex = make_tex(DXGI_FORMAT_R8_UNORM, w, h, D3D11_BIND_RENDER_TARGET.0 as u32)?;
        let mut out = None;
        unsafe { device.CreateRenderTargetView(&y_tex, None, Some(&mut out as *mut _))? };
        let y_rtv = out.unwrap();

        let uv_tex = make_tex(DXGI_FORMAT_R8G8_UNORM, w / 2, h / 2, D3D11_BIND_RENDER_TARGET.0 as u32)?;
        let mut out = None;
        unsafe { device.CreateRenderTargetView(&uv_tex, None, Some(&mut out as *mut _))? };
        let uv_rtv = out.unwrap();

        // 双平面 staging：Y(R8 全分辨率) + UV(R8G8 半分辨率)，CPU 拼 NV12
        let make_staging = |fmt, tw, th| -> windows::core::Result<ID3D11Texture2D> {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: tw,
                Height: th,
                MipLevels: 1,
                ArraySize: 1,
                Format: fmt,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut t: Option<ID3D11Texture2D> = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut t as *mut _))? };
            t.ok_or(windows::core::Error::from_hresult(windows::core::HRESULT(-1)))
        };
        let staging_y = make_staging(DXGI_FORMAT_R8_UNORM, w, h)?;
        let staging_uv = make_staging(DXGI_FORMAT_R8G8_UNORM, w / 2, h / 2)?;

        Ok(Self {
            device: device.clone(),
            ctx,
            vs,
            ps_y,
            ps_uv,
            sampler,
            rs,
            bgra_tex,
            bgra_srv,
            y_tex,
            y_rtv,
            uv_tex,
            uv_rtv,
            staging_y,
            staging_uv,
            w,
            h,
            scratch: Vec::new(),
            pitch: 0,
            frame_srv_ok: true,
            probe_n: 0,
        })
    }

    /// BGRA 帧纹理 → GPU 转 NV12 → 回读到 CPU。返回借用内部 scratch 的视图。
    pub fn convert_and_readback(&mut self, frame_tex: &ID3D11Texture2D) -> windows::core::Result<Nv12View<'_>> {
        unsafe {
            let ctx = &self.ctx;
            // 图元拓扑：默认 UNDEFINED 下 Draw 无效；关剔除：默认 CullBack 会剔掉 CCW 全屏三角形
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.RSSetState(&self.rs);
            // 1) 源 SRV：优先直接用 WGC 帧纹理（省每帧 8MB 中转拷贝）；
            //    帧纹理不带 SHADER_RESOURCE 绑定时回退自有 BGRA 中转（并记日志）
            let srv: ID3D11ShaderResourceView = if self.frame_srv_ok {
                let mut s: Option<ID3D11ShaderResourceView> = None;
                match self.device.CreateShaderResourceView(frame_tex, None, Some(&mut s as *mut _)) {
                    Ok(()) => match s {
                        Some(srv) => srv,
                        None => {
                            eprintln!("[gpu] WGC 帧纹理不可建 SRV → 改走 BGRA 中转拷贝");
                            self.frame_srv_ok = false;
                            ctx.CopyResource(&self.bgra_tex, frame_tex);
                            self.bgra_srv.clone()
                        }
                    },
                    Err(e) => {
                        eprintln!("[gpu] WGC 帧纹理建 SRV 失败({e}) → 改走 BGRA 中转拷贝");
                        self.frame_srv_ok = false;
                        ctx.CopyResource(&self.bgra_tex, frame_tex);
                        self.bgra_srv.clone()
                    }
                }
            } else {
                ctx.CopyResource(&self.bgra_tex, frame_tex);
                self.bgra_srv.clone()
            };

            // 2) Y pass：全分辨率
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps_y, None);
            ctx.PSSetShaderResources(0, Some(&[Some(srv)]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.OMSetRenderTargets(Some(&[Some(self.y_rtv.clone())]), None);
            ctx.RSSetViewports(Some(&[viewport(self.w, self.h)]));
            ctx.Draw(3, 0);

            // 3) UV pass：半分辨率（换 PS + RT + 视口即可，SRV/采样器/VS 沿用）
            ctx.PSSetShader(&self.ps_uv, None);
            ctx.OMSetRenderTargets(Some(&[Some(self.uv_rtv.clone())]), None);
            ctx.RSSetViewports(Some(&[viewport(self.w / 2, self.h / 2)]));
            ctx.Draw(3, 0);
            ctx.OMSetRenderTargets(None, None);

            // 4) 双平面回读 + CPU 拼 NV12（不碰 NV12 纹理/平面拷贝——Intel 驱动坑）
            //    全纹理 CopyResource（DEFAULT→STAGING 同格式）是最基础的操作，驱动兼容性无忧
            ctx.OMSetRenderTargets(None, None);
            ctx.CopyResource(&self.staging_y, &self.y_tex);
            ctx.CopyResource(&self.staging_uv, &self.uv_tex);
            let mut my = D3D11_MAPPED_SUBRESOURCE::default();
            let mut muv = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.staging_y, 0, D3D11_MAP_READ, 0, Some(&mut my))?;
            ctx.Map(&self.staging_uv, 0, D3D11_MAP_READ, 0, Some(&mut muv))?;
            let (pitch, h, w) = (my.RowPitch as usize, self.h as usize, self.w as usize);
            let y_src = std::slice::from_raw_parts(my.pData as *const u8, pitch * h);
            let uv_pitch = muv.RowPitch as usize;
            let uv_src = std::slice::from_raw_parts(muv.pData as *const u8, uv_pitch * (h / 2));

            // 探针（每 90 帧）：Y/UV 非零占比——全零即 GPU 链路产出废数据
            self.probe_n += 1;
            if self.probe_n % 90 == 1 {
                let nz = |s: &[u8]| s.iter().filter(|&&b| b != 0).count() * 100 / s.len().max(1);
                let (y_len, uv_len) = (pitch * h, uv_pitch * (h / 2));
                println!(
                    "[gpu] 探针#{}: Y 非零 {}% / UV 非零 {}%（{} 路径）",
                    self.probe_n,
                    nz(&y_src[..y_len]),
                    nz(&uv_src[..uv_len]),
                    if self.frame_srv_ok { "直连SRV" } else { "BGRA中转" },
                );
            }

            self.scratch.clear();
            self.scratch.resize(pitch * h * 3 / 2, 0);
            let dst = self.scratch.as_mut_slice();
            for r in 0..h {
                dst[r * pitch..r * pitch + w]
                    .copy_from_slice(&y_src[r * pitch..r * pitch + w]);
            }
            let uv_off = pitch * h;
            for r in 0..h / 2 {
                // R8G8 行 = Cb,Cr 交错 = NV12 UV 行，原样搬运
                dst[uv_off + r * pitch..uv_off + r * pitch + w]
                    .copy_from_slice(&uv_src[r * uv_pitch..r * uv_pitch + w]);
            }
            self.pitch = pitch;
            ctx.Unmap(&self.staging_y, 0);
            ctx.Unmap(&self.staging_uv, 0);
        }
        let buf = self.scratch.as_slice();
        Ok(Nv12View { buf, pitch: self.pitch })
    }
}
