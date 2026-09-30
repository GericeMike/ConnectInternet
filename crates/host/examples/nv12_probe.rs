//! M2-T2.1 前置验证(实证版):WGC 帧池在 Win10 19045 上能否用 NV12 格式。
//! 判定:
//!   - `FramePool::Create(NV12)` 失败,或出帧后纹理格式 ≠ NV12 → T2 死路,走 T3 零拷贝
//!   - Create 成功 + 帧到达 + 格式 == NV12 → T2 活,fork windows-capture 加枚举
//! 运行:`cargo run --release -p rdlink-host --example nv12_probe`

use windows::core::{factory, Interface};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITOR_DEFAULTTOPRIMARY, MONITORINFO};
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{GetDesktopWindow, SetCursorPos};

fn main() -> windows::core::Result<()> {
    unsafe {
        let _mta = CoIncrementMTAUsage()?;

        // D3D11 设备(硬适配,核显)
        let levels = [
            D3D_FEATURE_LEVEL(0xb100),
            D3D_FEATURE_LEVEL(0xb000),
            D3D_FEATURE_LEVEL(0xa100),
            D3D_FEATURE_LEVEL(0xa000),
        ];
        let mut d3d_device: Option<ID3D11Device> = None;
        let mut level = D3D_FEATURE_LEVEL::default();
        let mut ctx: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut d3d_device),
            Some(&mut level),
            Some(&mut ctx),
        )?;
        let d3d_device = d3d_device.unwrap();

        // WinRT IDirect3DDevice
        let dxgi: IDXGIDevice = d3d_device.cast()?;
        let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?;
        let device: IDirect3DDevice = inspectable.cast()?;

        // 主显示器 capture item
        let hmon = MonitorFromWindow(GetDesktopWindow(), MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(hmon, &mut mi).ok()?;
        let size = SizeInt32 {
            Width: mi.rcMonitor.right - mi.rcMonitor.left,
            Height: mi.rcMonitor.bottom - mi.rcMonitor.top,
        };
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop.CreateForMonitor(hmon)?;
        println!("主显示器: {}x{}", size.Width, size.Height);

        // ── 关键测试:NV12 帧池(DXGI_FORMAT_NV12 = 103)──
        let nv12 = DirectXPixelFormat(103);
        let frame_pool = match Direct3D11CaptureFramePool::Create(&device, nv12, 2, size) {
            Ok(p) => {
                println!("[1] FramePool::Create(NV12) 成功");
                p
            }
            Err(e) => {
                println!("[1] FramePool::Create(NV12) 失败: {e}");
                println!("==== 结论:NV12 帧池创建即被拒 → T2 死路,走 T3 零拷贝 ====");
                return Ok(());
            }
        };

        // 会话 + 出帧验证(3 秒窗口,光标微推制造脏区)
        let session = frame_pool.CreateCaptureSession(&item)?;
        session.StartCapture()?;
        println!("[2] 会话已启动,推光标并等帧 3 秒…");
        let _ = SetCursorPos(600, 400);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut verdict_printed = false;
        while std::time::Instant::now() < deadline {
            if let Ok(frame) = frame_pool.TryGetNextFrame() {
                if let Ok(surface) = frame.Surface() {
                    let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
                    let tex: ID3D11Texture2D = access.GetInterface()?;
                    let mut desc = Default::default();
                    tex.GetDesc(&mut desc);
                    let fmt: DXGI_FORMAT = desc.Format.into();
                    println!("[3] 帧到达,实际纹理格式 = {:?} (0x{:X}; 103=NV12, 87=BGRA8)", fmt, desc.Format.0);
                    let is_nv12 = desc.Format.0 == 103;
                    println!(
                        "==== 结论:{} ====",
                        if is_nv12 {
                            "NV12 直采可行!fork windows-capture 加枚举 + 双平面 readback,T2 主线放行"
                        } else {
                            "帧池接受了请求但实际给的是别的格式(被静默替换)→ T2 不可靠,走 T3"
                        }
                    );
                    verdict_printed = true;
                }
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !verdict_printed {
            println!("[3] 3 秒无帧到达(NV12 帧池可能静默失效)→ 走 T3 零拷贝更稳");
        }
        let _ = session.Close();
        let _ = frame_pool.Close();
        Ok(())
    }
}
