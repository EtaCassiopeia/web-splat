/*
    This file implements a gpu version of radix sort. A good introduction to general purpose radix sort can
    be found here: http://www.codercorner.com/RadixSortRevisited.htm

    The gpu radix sort implemented here is a reimplementation of the vulkan radix sort found in the fuchsia repos: https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/src/graphics/lib/compute/radix_sort/
    Currently only the sorting for floating point key-value pairs is implemented, as only this is needed for this project

    All shaders can be found in shaders/radix_sort.wgsl
*/

use wgpu::{ComputePassDescriptor, util::DeviceExt};

use crate::{
    camera::{Camera},
    uniform::UniformBuffer,
};

// IMPORTANT: the following constants have to be synced with the numbers in radix_sort.wgsl
const histogram_wg_size: usize = 256;
const rs_radix_log2: usize = 8;                 // 8 bit radices
const rs_radix_size: usize = 1 << rs_radix_log2;// 256 entries into the radix table
const rs_keyval_size: usize = 32 / rs_radix_log2;
const rs_histogram_block_rows : usize = 15;
const rs_scatter_block_rows : usize = rs_histogram_block_rows; // DO NOT CHANGE, shader assume this automatically
const prefix_wg_size: usize = 1 << 7;           // one thread operates on 2 prefixes at the same time
const scatter_wg_size: usize = 1 << 8;


pub struct GPURSSorter {
    pub bind_group_layout: wgpu::BindGroupLayout,
    zero_p:         wgpu::ComputePipeline,
    histogram_p:    wgpu::ComputePipeline,
    prefix_p:       wgpu::ComputePipeline,
    scatter_even_p: wgpu::ComputePipeline,
    scatter_odd_p : wgpu::ComputePipeline,
    subgroup_size:  usize,
}

pub struct GeneralInfo{
    pub histogram_size: u32,
    pub keys_size:      u32,
    pub padded_size:    u32,
    pub passes:         u32,
    pub even_pass:      u32,
    pub odd_pass:       u32,
}

unsafe fn any_as_u8_slice<T: Sized>(p: &T) -> &[u8] {
    ::core::slice::from_raw_parts((p as *const T) as *const u8, ::core::mem::size_of::<T>(),)
}

impl GPURSSorter{
    // The new call also needs the queue to be able to determine the maximum subgroup size (Does so by running test runs)
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        println!("Searching for the maximum subgroup size (wgpu currently does not allow to query subgroup sizes)");
        let sizes = vec![1, 16, 32, 64, 128];
        let mut cur_size = 2;
        let mut cur_sorter = Self::new_with_sg_size(device, sizes[cur_size]);
        enum state {init, increasing, decreasing};
        let mut s = state::init;
        while true {
            if cur_size < 0 || cur_size >= sizes.len() {
                panic!("GPURSSorter::new() No workgroup size that works was found. Unable to use sorter");
            }
            println!("Checking sorting with subgroupsize {}", sizes[cur_size]);
            cur_sorter = Self::new_with_sg_size(device, sizes[cur_size]);
            let sort_success = cur_sorter.test_sort(device, queue);
            match s {
                state::init =>
                    if sort_success {s = state::increasing; cur_size += 1;}
                    else {s = state::decreasing; cur_size -= 1;}
                state::increasing =>
                    if sort_success {cur_size += 1;}
                    else {cur_sorter = Self::new_with_sg_size(device, sizes[cur_size - 1]); break;}
                state::decreasing =>
                    if sort_success {break;}
                    else {cur_size -= 1;}
            }
        }
        println!("Created a sorter with subgroup size {}", cur_sorter.subgroup_size);
        return cur_sorter;
    }
    
    fn new_with_sg_size(device: &wgpu::Device, sg_size: i32) -> Self{
        // special variables for scatter shade
        let histogram_sg_size : usize = sg_size as usize;
        let rs_sweep_0_size : usize = rs_radix_size / histogram_sg_size;
        let rs_sweep_1_size : usize = rs_sweep_0_size / histogram_sg_size;
        let rs_sweep_2_size : usize = rs_sweep_1_size / histogram_sg_size;
        let rs_sweep_size : usize = rs_sweep_0_size + rs_sweep_1_size + rs_sweep_2_size;
        let rs_smem_phase_1 : usize = rs_radix_size + rs_radix_size + rs_sweep_size;
        let rs_smem_phase_2 : usize = rs_radix_size + rs_scatter_block_rows * {scatter_wg_size};
        // rs_smem_phase_2 will always be larger, so always use phase2
        let rs_mem_dwords : usize = rs_smem_phase_2;
        let rs_mem_sweep_0_offset : usize = 0;
        let rs_mem_sweep_1_offset : usize = rs_mem_sweep_0_offset + rs_sweep_0_size;
        let rs_mem_sweep_2_offset : usize = rs_mem_sweep_1_offset + rs_sweep_1_size;
        let bind_group_layout =device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor{
                    label: Some("Radix bind group layout"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::COMPUTE,
                            ty: wgpu::BindingType::Buffer { 
                                ty: wgpu::BufferBindingType::Storage { read_only: false } , 
                                has_dynamic_offset: false,
                                min_binding_size: None 
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::COMPUTE,
                            ty: wgpu::BindingType::Buffer { 
                                ty: wgpu::BufferBindingType::Storage { read_only: false },
                                has_dynamic_offset: false, 
                                min_binding_size: None 
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 2,
                            visibility: wgpu::ShaderStages::COMPUTE,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Storage {read_only: false },
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 3,
                            visibility: wgpu::ShaderStages::COMPUTE,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Storage {read_only: false },
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                    ]
                });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("preprocess pipeline layout"),
            bind_group_layouts: &[ &bind_group_layout ],
            push_constant_ranges: &[],
        });

        const raw_shader : &str = include_str!("shaders/radix_sort.wgsl");
        let shader_w_const = format!("const histogram_sg_size: u32 = {:}u;\n\
                                            const histogram_wg_size: u32 = {:}u;\n\
                                            const rs_radix_log2: u32 = {:}u;\n\
                                            const rs_radix_size: u32 = {:}u;\n\
                                            const rs_keyval_size: u32 = {:}u;\n\
                                            const rs_histogram_block_rows: u32 = {:}u;\n\
                                            const rs_scatter_block_rows: u32 = {:}u;\n\
                                            const rs_mem_dwords: u32 = {:}u;\n\
                                            const rs_mem_sweep_0_offset: u32 = {:}u;\n\
                                            const rs_mem_sweep_1_offset: u32 = {:}u;\n\
                                            const rs_mem_sweep_2_offset: u32 = {:}u;\n{:}", histogram_sg_size, histogram_wg_size, rs_radix_log2, rs_radix_size, rs_keyval_size, rs_histogram_block_rows, rs_scatter_block_rows, 
                                            rs_mem_dwords, rs_mem_sweep_0_offset, rs_mem_sweep_1_offset, rs_mem_sweep_2_offset, raw_shader);
        let shader_code = shader_w_const.replace("{histogram_wg_size}", histogram_wg_size.to_string().as_str())
            .replace("{prefix_wg_size}", prefix_wg_size.to_string().as_str())
            .replace("{scatter_wg_size}", scatter_wg_size.to_string().as_str());
        // println!("{}", shader_code);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Radix sort shader"),
            source: wgpu::ShaderSource::Wgsl(shader_code.into()),
        });
        let zero_p = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("Zero the histograms"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: "zero_histograms",
        });
        let histogram_p = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("calculate_histogram"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: "calculate_histogram",
        });
        let prefix_p = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("prefix_histogram"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: "prefix_histogram",
        });
        let scatter_even_p = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("scatter_even"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: "scatter_even",
        });
        let scatter_odd_p = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("scatter_odd"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: "scatter_odd",
        });

        return Self { bind_group_layout, zero_p, histogram_p, prefix_p, scatter_even_p, scatter_odd_p , subgroup_size: histogram_sg_size };
    }
    
    fn test_sort(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> bool {
        // smiply runs a small sort and check if the sorting result is correct
        let n = 512;    // means that 2 workgroups are needed for sorting
        let scrambled_data : Vec<f32> = (0..n).rev().map(|x| x as f32).collect();
        let sorted_data : Vec<f32> = (0..n).map(|x| x as f32).collect();

        let internal_mem_buffer = Self::create_internal_mem_buffer(self, device, n);
        let (keyval_a, keyval_b) = Self::create_keyval_buffers(device, n);
        let (uniform_buffer, bind_group) = self.create_bind_group(device, n, &internal_mem_buffer, &keyval_a, &keyval_b);

        upload_to_buffer(&keyval_a, device, queue, scrambled_data.as_slice());
        
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {label: Some("GPURSSorter test_sort")});
        self.record_sort(&bind_group, n, &mut encoder);
        queue.submit([encoder.finish()]);
        device.poll(wgpu::Maintain::Wait);
        
        let sorted = pollster::block_on(download_buffer::<f32>(&keyval_a, device, queue));
        for i in 0..n {
            if sorted[i] != sorted_data[i] {
                return false;
            }
        }
        return true;
    }
    
    fn get_scatter_histogram_sizes(keysize: usize) -> (usize, usize, usize, usize, usize, usize) {
        let scatter_block_kvs = histogram_wg_size * rs_scatter_block_rows;
        let scatter_blocks_ru = (keysize + scatter_block_kvs - 1) / scatter_block_kvs;
        let count_ru_scatter = scatter_blocks_ru * scatter_block_kvs;
        
        let histo_block_kvs = histogram_wg_size * rs_histogram_block_rows;
        let histo_blocks_ru = (count_ru_scatter + histo_block_kvs - 1) / histo_block_kvs;
        let count_ru_histo = histo_blocks_ru * histo_block_kvs;
        
        return (scatter_block_kvs, scatter_blocks_ru, count_ru_scatter, histo_block_kvs, histo_blocks_ru, count_ru_histo);
    }
    
    pub fn create_keyval_buffers(device: &wgpu::Device, keysize: usize) -> (wgpu::Buffer, wgpu::Buffer) {
        let (scatter_block_kvs, scatter_blocks_ru, count_ru_scatter, histo_block_kvs, hist_blocks_ru, count_ru_histo) = Self::get_scatter_histogram_sizes(keysize);

        // creating the two needed buffers for sorting
        let buffer_a = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Radix data buffer a"),
            size: (count_ru_histo * std::mem::size_of::<f32>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let buffer_b = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Radix data buffer a"),
            size: (count_ru_histo * std::mem::size_of::<f32>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        return (buffer_a, buffer_b);
    }
    
    // caclulates and allocates a buffer that is sufficient for holding all needed information for
    // sorting. This includes the histograms and the temporary scatter buffer
    // @return: tuple containing [internal memory buffer (should be bound at shader binding 1, count_ru_histo (padded size needed for the keyval buffer)]
    pub fn create_internal_mem_buffer(&self, device: &wgpu::Device, keysize: usize) -> wgpu::Buffer {
        // currently only a few different key bits are supported, maybe has to be extended
        // assert!(key_bits == 32 || key_bits == 64 || key_bits == 16);
        
        // subgroup and workgroup sizes
        let histo_sg_size : usize = self.subgroup_size;
        let histo_wg_size : usize = histogram_wg_size;
        let prefix_sg_size : usize = histo_sg_size;
        let internal_sg_size : usize = histo_sg_size;

        // The "internal" memory map looks like this:
        //
        //   +---------------------------------+ <-- 0
        //   | histograms[keyval_size]         |
        //   +---------------------------------+ <-- keyval_size                           * histo_size
        //   | partitions[scatter_blocks_ru-1] |
        //   +---------------------------------+ <-- (keyval_size + scatter_blocks_ru - 1) * histo_size
        //   | workgroup_ids[keyval_size]      |
        //   +---------------------------------+ <-- (keyval_size + scatter_blocks_ru - 1) * histo_size + workgroup_ids_size
        
        let (scatter_block_kvs, scatter_blocks_ru, count_ru_scatter, histo_block_kvs, hist_blocks_ru, count_ru_histo) = Self::get_scatter_histogram_sizes(keysize);

        let mr_keyval_size = rs_keyval_size * count_ru_histo;
        let mr_keyvals_align = rs_keyval_size * histo_sg_size;
        
        let histo_size = rs_radix_size * std::mem::size_of::<u32>();

        let mut internal_size= (rs_keyval_size + scatter_blocks_ru - 1) * histo_size;
        let internal_alignment = internal_sg_size * std::mem::size_of::<u32>();
        
        // println!("Created buffer for {keysize} keys, count_ru_scatter {count_ru_scatter}, count_ru_histo {count_ru_histo}, mr_keyval_size {mr_keyval_size}, histo_size {histo_size}");
        // println!("internal_size {internal_size}");
        
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Internal radix sort buffer"),
            size: internal_size as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false
        });
        return buffer;
    }
    
    pub fn create_bind_group(&self, device: &wgpu::Device , keysize: usize, internal_mem_buffer: &wgpu::Buffer, keyval_a: &wgpu::Buffer, keyval_b: &wgpu::Buffer) -> (wgpu::Buffer, wgpu::BindGroup){
        let (scatter_block_kvs, scatter_blocks_ru, count_ru_scatter, histo_block_kvs, hist_blocks_ru, count_ru_histo) = Self::get_scatter_histogram_sizes(keysize);
        if keyval_a.size() as usize != count_ru_histo * std::mem::size_of::<f32>() || keyval_b.size() as usize != count_ru_histo * std::mem::size_of::<f32>() {
            panic!("Keyval buffers are not padded correctly. Were they created with GPURSSorter::create_keyval_buffers()");
        }
        let uniform_infos = GeneralInfo{histogram_size: 0, keys_size: keysize as u32, padded_size: count_ru_histo as u32, passes: 4, even_pass: 0, odd_pass: 0};
        let uniform_buffer= device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Radix uniform buffer"),
            contents: unsafe{any_as_u8_slice(&uniform_infos)},
            usage: wgpu::BufferUsages::STORAGE,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Radix bind group"),
            layout: &self.bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: internal_mem_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: keyval_a.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: keyval_b.as_entire_binding(),
            }
            ]
        });
        return (uniform_buffer, bind_group);
    }
    
    pub fn record_calculate_histogram(&self, bind_group: &wgpu::BindGroup, keysize: usize, encoder: &mut wgpu::CommandEncoder) {
        // histogram has to be zeroed out such that counts that might have been done in the past are erased and do not interfere with the new count
        // encoder.clear_buffer(histogram_buffer, 0, None);
        
        // as we only deal with 32 bit float values always 4 passes are conducted
        let (scatter_block_kvs, scatter_blocks_ru, count_ru_scatter, histo_block_kvs, hist_blocks_ru, count_ru_histo) = Self::get_scatter_histogram_sizes(keysize);
        const passes: u32 = 4;

        // let count_ru_histo = histo_blocks_ru * histo_block_kvs;
        
        let histo_size = rs_radix_size;
        
        {
            let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor {label: Some("zeroing the histogram")});
            
            pass.set_pipeline(&self.zero_p);
            pass.set_bind_group(0, bind_group, &[]);
            let n = (rs_keyval_size + scatter_blocks_ru - 1) * histo_size + if count_ru_histo > keysize {count_ru_histo - keysize} else {0};
            let dispatch = ((n as f32 / histogram_wg_size as f32)).ceil() as u32;
            pass.dispatch_workgroups(dispatch, 1, 1);
        }

        {
            let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor {label:Some("calculate histogram")});

            pass.set_pipeline(&self.histogram_p);
            pass.set_bind_group(0, bind_group, &[]);
            pass.dispatch_workgroups(hist_blocks_ru as u32, 1, 1);
        }
    }
    
    pub fn record_prefix_histogram(&self, bind_group: &wgpu::BindGroup, passes: usize, encoder: &mut wgpu::CommandEncoder) {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {label: Some("prefix histogram")});

        pass.set_pipeline(&self.prefix_p);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(passes as u32, 1, 1);
    }
    
    pub fn record_scatter_keys(&self, bind_group: &wgpu::BindGroup, passes: usize, keysize: usize, encoder: &mut wgpu::CommandEncoder) {
        assert!(passes == 4);   // currently the amount of passes is hardcoded in the shader
        let (_, scatter_blocks_ru, _, _, _, _) = Self::get_scatter_histogram_sizes(keysize);
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {label: Some("Scatter keyvals")});
        
        pass.set_bind_group(0, bind_group, &[]);
        pass.set_pipeline(&self.scatter_even_p);
        pass.dispatch_workgroups(scatter_blocks_ru as u32, 1, 1);
        
        pass.set_pipeline(&self.scatter_odd_p);
        pass.dispatch_workgroups(scatter_blocks_ru as u32, 1, 1);

        pass.set_pipeline(&self.scatter_even_p);
        pass.dispatch_workgroups(scatter_blocks_ru as u32, 1, 1);

        pass.set_pipeline(&self.scatter_odd_p);
        pass.dispatch_workgroups(scatter_blocks_ru as u32, 1, 1);
    }
    
    pub fn record_sort(&self, bind_group: &wgpu::BindGroup, keysize: usize, encoder: &mut wgpu::CommandEncoder) {
        self.record_calculate_histogram(&bind_group, keysize, encoder);
        self.record_prefix_histogram(&bind_group, 4, encoder);
        self.record_scatter_keys(&bind_group, 4, keysize, encoder);
    }
}

fn upload_to_buffer<T: bytemuck::Pod>(buffer: &wgpu::Buffer, device : &wgpu::Device, queue: &wgpu::Queue, values: &[T]){
    let staging_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("Staging buffer"),
        contents: bytemuck::cast_slice(values),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {label: Some("Copye endoder")});
    encoder.copy_buffer_to_buffer(&staging_buffer, 0, buffer, 0, staging_buffer.size());
    queue.submit([encoder.finish()]);
    
    device.poll(wgpu::Maintain::Wait);
    staging_buffer.destroy();
}

async fn download_buffer<T: Clone>(buffer: &wgpu::Buffer, device: &wgpu::Device, queue: &wgpu::Queue) -> Vec<T>{
    // copy buffer data
    let download_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Download buffer"),
        size: buffer.size(),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder= device.create_command_encoder(&wgpu::CommandEncoderDescriptor {label: Some("Copy encoder")});
    encoder.copy_buffer_to_buffer(buffer, 0, &download_buffer, 0, buffer.size());
    queue.submit([encoder.finish()]);
    
    // download buffer
    let buffer_slice = download_buffer.slice(..);
    let (tx, rx) = futures_intrusive::channel::shared::oneshot_channel();
    buffer_slice.map_async(wgpu::MapMode::Read, move |result| tx.send(result).unwrap());
    device.poll(wgpu::Maintain::Wait);
    rx.receive().await.unwrap().unwrap();
    let data = buffer_slice.get_mapped_range();
    let mut r;
    
    unsafe {
        let (prefix, d, suffix) = data.align_to::<T>();
        r = d.to_vec();
    }
    
    download_buffer.destroy();
    
    return r;
}