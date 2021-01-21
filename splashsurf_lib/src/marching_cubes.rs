use crate::marching_cubes_lut::marching_cubes_triangulation_iter;
use crate::mesh::TriMesh3d;
use crate::topology::{Axis, DirectedAxis, DirectedAxisArray, Direction};
use crate::uniform_grid::{GridBoundaryFaceFlags, PointIndex, SubdomainGrid};
use crate::{new_map, DensityMap, Index, MapType, Real, UniformGrid};
use anyhow::Context;
use log::info;
use nalgebra::Vector3;

// TODO: Merge the three interpolate implementations
// TODO: Avoid the index conversions by directly using global indices

/// Performs a marching cubes triangulation of a density map on the given background grid
pub fn triangulate_density_map<I: Index, R: Real>(
    grid: &UniformGrid<I, R>,
    density_map: &DensityMap<I, R>,
    iso_surface_threshold: R,
) -> TriMesh3d<R> {
    profile!("triangulate_density_map");

    let mut mesh = TriMesh3d::default();
    let marching_cubes_data = interpolate_points_to_cell_data::<I, R>(
        &grid,
        &density_map,
        iso_surface_threshold,
        &mut mesh.vertices,
    );
    triangulate::<I, R>(marching_cubes_data, &mut mesh);
    mesh
}

/// Performs a marching cubes triangulation of a density map on the given background grid, appends triangles to the given mesh
pub fn triangulate_density_map_append<I: Index, R: Real>(
    grid: &UniformGrid<I, R>,
    subdomain_offset: Option<&PointIndex<I>>,
    subdomain_grid: Option<&UniformGrid<I, R>>,
    density_map: &DensityMap<I, R>,
    iso_surface_threshold: R,
    mesh: &mut TriMesh3d<R>,
) {
    profile!("triangulate_density_map_append");

    if let (Some(subdomain_grid), Some(subdomain_offset)) = (subdomain_grid, subdomain_offset) {
        let subdomain = SubdomainGrid::new(
            grid.clone(),
            subdomain_grid.clone(),
            subdomain_offset.index().clone(),
        );

        let (marching_cubes_data, _) = interpolate_points_to_cell_data_skip_boundary::<I, R>(
            &subdomain,
            &density_map,
            iso_surface_threshold,
            &mut mesh.vertices,
        );

        triangulate_with_criterion(
            &subdomain,
            marching_cubes_data,
            mesh,
            TriangulationSkipBoundaryCells,
            DefaultTriangleGenerator,
        );
    } else {
        let marching_cubes_data = interpolate_points_to_cell_data::<I, R>(
            &grid,
            &density_map,
            iso_surface_threshold,
            &mut mesh.vertices,
        );
        triangulate::<I, R>(marching_cubes_data, mesh);
    }
}

pub(crate) fn triangulate_density_map_with_stitching_data<I: Index, R: Real>(
    grid: &UniformGrid<I, R>,
    subdomain_offset: &PointIndex<I>,
    subdomain_grid: &UniformGrid<I, R>,
    density_map: &DensityMap<I, R>,
    iso_surface_threshold: R,
) -> SurfacePatch<I, R> {
    profile!("triangulate_density_map_append");

    let mut mesh = TriMesh3d::default();

    let subdomain = SubdomainGrid::new(
        grid.clone(),
        subdomain_grid.clone(),
        subdomain_offset.index().clone(),
    );

    let (marching_cubes_data, mut boundary_density_maps) =
        interpolate_points_to_cell_data_skip_boundary::<I, R>(
            &subdomain,
            &density_map,
            iso_surface_threshold,
            &mut mesh.vertices,
        );

    let mut boundary_cell_data = collect_boundary_cell_data(&subdomain, &marching_cubes_data);

    triangulate_with_criterion(
        &subdomain,
        marching_cubes_data,
        &mut mesh,
        TriangulationSkipBoundaryCells,
        DefaultTriangleGenerator,
    );

    SurfacePatch {
        mesh,
        subdomain,
        data: DirectedAxisArray::new_with(|axis| BoundaryData {
            boundary_density_map: std::mem::take(boundary_density_maps.get_mut(axis)),
            boundary_cell_data: std::mem::take(boundary_cell_data.get_mut(axis)),
        }),
        stitching_level: 0,
    }
}

/// Flag indicating whether a vertex is above or below the iso-surface
#[derive(Copy, Clone, Debug)]
enum RelativeToThreshold {
    Below,
    Indeterminate,
    Above,
}

impl RelativeToThreshold {
    /// Returns if the value is above the iso-surface, panics if the value is indeterminate
    fn is_above(&self) -> bool {
        match self {
            RelativeToThreshold::Below => false,
            RelativeToThreshold::Above => true,
            // TODO: Replace with error?
            RelativeToThreshold::Indeterminate => panic!(),
        }
    }
}

/// Data for a single cell required by marching cubes
#[derive(Clone, Debug)]
pub(crate) struct CellData {
    /// The interpolated iso-surface vertex per edge if the edge crosses the iso-surface
    iso_surface_vertices: [Option<usize>; 12],
    /// Flags indicating whether a corner vertex is above or below the iso-surface threshold
    corner_above_threshold: [RelativeToThreshold; 8],
}

impl CellData {
    /// Returns an boolean array indicating for each corner vertex of the cell whether it's above the iso-surface
    fn are_vertices_above(&self) -> [bool; 8] {
        [
            self.corner_above_threshold[0].is_above(),
            self.corner_above_threshold[1].is_above(),
            self.corner_above_threshold[2].is_above(),
            self.corner_above_threshold[3].is_above(),
            self.corner_above_threshold[4].is_above(),
            self.corner_above_threshold[5].is_above(),
            self.corner_above_threshold[6].is_above(),
            self.corner_above_threshold[7].is_above(),
        ]
    }
}

impl Default for CellData {
    fn default() -> Self {
        CellData {
            iso_surface_vertices: [None; 12],
            corner_above_threshold: [RelativeToThreshold::Indeterminate; 8],
        }
    }
}

/// Input for the marching cubes algorithm
#[derive(Clone, Debug)]
pub(crate) struct MarchingCubesInput<I: Index> {
    /// Data for all cells that marching cubes has to visit
    cell_data: MapType<I, CellData>,
}

/// Generates input data for performing the actual marching cubes triangulation
///
/// The returned data is a map of all cells that have to be visited by marching cubes.
/// For each cell, it is stored whether the corner vertices are above/below the iso-surface
/// threshold and the indices of the interpolated vertices for each edge that crosses the iso-surface.
///
/// The interpolated vertices are appended to the given vertex vector.
#[inline(never)]
pub(crate) fn interpolate_points_to_cell_data<I: Index, R: Real>(
    grid: &UniformGrid<I, R>,
    density_map: &DensityMap<I, R>,
    iso_surface_threshold: R,
    vertices: &mut Vec<Vector3<R>>,
) -> MarchingCubesInput<I> {
    profile!("interpolate_points_to_cell_data");

    // Note: This functions assumes that the default value for missing point data is below the iso-surface threshold
    info!("Starting interpolation of cell data for marching cubes...");

    // Map from flat cell index to all data that is required per cell for the marching cubes triangulation
    let mut cell_data: MapType<I, CellData> = new_map();

    // Generate iso-surface vertices and identify affected cells & edges
    {
        profile!("generate_iso_surface_vertices");
        density_map.for_each(|flat_point_index, point_value| {
            // We want to find edges that cross the iso-surface,
            // therefore we can choose to either skip all points above or below the threshold.
            //
            // In most scenes, the sparse density map should contain more entries above than
            // below the threshold, as it contains the whole fluid interior, whereas areas completely
            // devoid of fluid are not part of the density map.
            //
            // Therefore, we choose to skip points with densities above the threshold to improve efficiency
            if point_value > iso_surface_threshold {
                return;
            }

            let point = grid.try_unflatten_point_index(flat_point_index)
                .expect("Flat point index does not belong to grid. You have to supply the same grid that was used to create the density map.");
            let neighborhood = grid.get_point_neighborhood(&point);

            // Iterate over all neighbors of the point to find edges crossing the iso-surface
            for neighbor_edge in neighborhood.neighbor_edge_iter() {
                let neighbor = neighbor_edge.neighbor_index();

                let flat_neighbor_index = grid.flatten_point_index(neighbor);
                // Try to read out the function value at the neighboring point
                let neighbor_value = if let Some(v) = density_map.get(flat_neighbor_index) {
                    v
                } else {
                    // Neighbors that are not in the point-value map were outside of the kernel evaluation radius.
                    // This should only happen for cells that are completely outside of the compact support of a particle.
                    // The point-value map has to be consistent such that for each cell, where at least one point-value
                    // is missing like this, the cell has to be completely below the iso-surface threshold.
                    continue;
                };

                // Check if an edge crossing the iso-surface was found
                if neighbor_value > iso_surface_threshold {
                    // Interpolate iso-surface vertex on the edge
                    let alpha =
                        (iso_surface_threshold - point_value) / (neighbor_value - point_value);
                    let point_coords = grid.point_coordinates(&point);
                    let neighbor_coords = grid.point_coordinates(neighbor);
                    let interpolated_coords =
                        (point_coords) * (R::one() - alpha) + neighbor_coords * alpha;

                    // Store interpolated vertex and remember its index
                    let vertex_index = vertices.len();
                    vertices.push(interpolated_coords);

                    // Store the data required for the marching cubes triangulation for
                    // each cell adjacent to the edge crossing the iso-surface.
                    // This includes the above/below iso-surface flags and the interpolated vertex index.
                    for cell in grid.cells_adjacent_to_edge(&neighbor_edge).iter().flatten() {
                        let flat_cell_index = grid.flatten_cell_index(cell);

                        let mut cell_data_entry = cell_data
                            .entry(flat_cell_index)
                            .or_insert_with(CellData::default);

                        // Store the index of the interpolated vertex on the corresponding local edge of the cell
                        let local_edge_index = cell.local_edge_index_of(&neighbor_edge).unwrap();
                        assert!(cell_data_entry.iso_surface_vertices[local_edge_index].is_none(), "Overwriting already existing vertex. This is a bug.");
                        cell_data_entry.iso_surface_vertices[local_edge_index] = Some(vertex_index);

                        // Mark the neighbor as above the iso-surface threshold
                        let local_vertex_index =
                            cell.local_point_index_of(neighbor.index()).unwrap();
                        cell_data_entry.corner_above_threshold[local_vertex_index] =
                            RelativeToThreshold::Above;
                    }
                }
            }
        });
    }

    // Cell corner points above the iso-surface threshold which are only surrounded by neighbors that
    // are also above the threshold were not marked as `corner_above_threshold = true` before, because they
    // don't have any adjacent edge crossing the iso-surface (and thus were never touched by the point data loop).
    // This can happen in a configuration where e.g. only one corner is below the threshold.
    //
    // Therefore, we have to loop over all corner points of all cells that were collected for marching cubes
    // and check their density value again.
    //
    // Note, that we would also have this problem if we flipped the default/initial value of corner_above_threshold
    // to false. In this case we could also move this into the point data loop (which might increase performance).
    // However, we would have to special case cells without point data, which are currently skipped.
    // Similarly, they have to be treated in a second pass because we don't want to initialize cells only
    // consisting of missing points and points below the surface.
    {
        profile!("relative_to_threshold_postprocessing");
        for (&flat_cell_index, cell_data) in cell_data.iter_mut() {
            let cell = grid.try_unflatten_cell_index(flat_cell_index).unwrap();
            for (local_point_index, flag_above) in
                cell_data.corner_above_threshold.iter_mut().enumerate()
            {
                // If the point is already marked as above we can ignore it
                if let RelativeToThreshold::Above = flag_above {
                    continue;
                }

                // Otherwise try to look up its value and potentially mark it as above the threshold
                let point = cell.global_point_index_of(local_point_index).unwrap();
                let flat_point_index = grid.flatten_point_index(&point);
                if let Some(point_value) = density_map.get(flat_point_index) {
                    if point_value > iso_surface_threshold {
                        *flag_above = RelativeToThreshold::Above;
                    } else {
                        *flag_above = RelativeToThreshold::Below;
                    }
                } else {
                    *flag_above = RelativeToThreshold::Below;
                }
            }
        }
    }

    #[cfg(debug_assertions)]
    assert_cell_data_point_data_consistency(density_map, &cell_data, grid, iso_surface_threshold);

    info!(
        "Generated cell data for marching cubes with {} cells and {} vertices.",
        cell_data.len(),
        vertices.len()
    );
    info!("Interpolation done.");

    MarchingCubesInput { cell_data }
}

#[inline(never)]
pub(crate) fn interpolate_points_to_cell_data_skip_boundary<I: Index, R: Real>(
    subdomain: &SubdomainGrid<I, R>,
    density_map: &DensityMap<I, R>,
    iso_surface_threshold: R,
    vertices: &mut Vec<Vector3<R>>,
) -> (MarchingCubesInput<I>, DirectedAxisArray<MapType<I, R>>) {
    let subdomain_grid = subdomain.subdomain_grid();

    assert!(
        subdomain_grid.cells_per_dim().iter().all(|&n_cells| n_cells > I::one() + I::one()),
        "Interpolation procedure with stitching support only works on grids & subdomains with more than 2 cells in each dimension!"
    );

    profile!("interpolate_points_to_cell_data_skip_boundary");

    // Note: This functions assumes that the default value for missing point data is below the iso-surface threshold
    info!("Starting interpolation of cell data for marching cubes...");

    // Map from flat cell index to all data that is required per cell for the marching cubes triangulation
    let mut cell_data: MapType<I, CellData> = new_map();

    // New density map for the boundary layer of this patch
    let mut boundary_density_maps: DirectedAxisArray<MapType<I, R>> = Default::default();

    // Closure to detect points that are on the outer boundary of the domain, edges towards these point should be skipped
    let point_is_on_outer_boundary = |p: &PointIndex<I>| -> bool {
        let point_boundary_flags = GridBoundaryFaceFlags::classify_point(subdomain_grid, p);
        !point_boundary_flags.is_empty()
    };

    // Generate iso-surface vertices and identify affected cells & edges
    {
        profile!("generate_iso_surface_vertices");
        density_map.for_each(|flat_point_index, point_value| {
            let point = subdomain_grid.try_unflatten_point_index(flat_point_index)
                .expect("Flat point index does not belong to grid. You have to supply the same grid that was used to create the density map.");

            // Skip points directly at the boundary but add them to the respective boundary density map
            {
                let point_boundary_flags = GridBoundaryFaceFlags::classify_point(subdomain_grid, &point);
                if !point_boundary_flags.is_empty() {
                    // Insert the point into each boundary density map it belongs to
                    for boundary in point_boundary_flags.iter_individual() {
                        let boundary_map = boundary_density_maps.get_mut(&boundary);
                        boundary_map.insert(flat_point_index, point_value);

                        // Also insert second row neighbor, if present
                        if let Some(flat_neighbor_index) = subdomain_grid
                            .get_point_neighbor(&point, boundary.opposite())
                            .map(|index| subdomain_grid.flatten_point_index(&index))
                        {
                            if let Some(density_value) = density_map.get(flat_neighbor_index) {
                                boundary_map.insert(flat_neighbor_index, density_value);
                            }
                        }
                    }
                    // Skip this point for interpolation
                    return;
                }
            }

            // We want to find edges that cross the iso-surface,
            // therefore we can choose to either skip all points above or below the threshold.
            //
            // In most scenes, the sparse density map should contain more entries above than
            // below the threshold, as it contains the whole fluid interior, whereas areas completely
            // devoid of fluid are not part of the density map.
            //
            // Therefore, we choose to skip points with densities above the threshold to improve efficiency
            if point_value > iso_surface_threshold {
                return;
            }

            let neighborhood = subdomain_grid.get_point_neighborhood(&point);
            // Iterate over all neighbors of the point to find edges crossing the iso-surface
            for neighbor_edge in neighborhood.neighbor_edge_iter() {
                let neighbor = neighbor_edge.neighbor_index();

                let flat_neighbor_index = subdomain_grid.flatten_point_index(neighbor);
                // Try to read out the function value at the neighboring point
                let neighbor_value = if let Some(v) = density_map.get(flat_neighbor_index) {
                    v
                } else {
                    // Neighbors that are not in the point-value map were outside of the kernel evaluation radius.
                    // This should only happen for cells that are completely outside of the compact support of a particle.
                    // The point-value map has to be consistent such that for each cell, where at least one point-value
                    // is missing like this, the cell has to be completely below the iso-surface threshold.
                    continue;
                };

                // Skip edges that don't cross the iso-surface
                if !(neighbor_value > iso_surface_threshold) {
                    continue;
                }

                // Skip edges that go into the boundary layer
                if point_is_on_outer_boundary(&neighbor) {
                    continue;
                }

                // Interpolate iso-surface vertex on the edge
                let alpha =
                    (iso_surface_threshold - point_value) / (neighbor_value - point_value);
                let point_coords = subdomain_grid.point_coordinates(&point);
                let neighbor_coords = subdomain_grid.point_coordinates(neighbor);
                let interpolated_coords =
                    (point_coords) * (R::one() - alpha) + neighbor_coords * alpha;

                // Store interpolated vertex and remember its index
                let vertex_index = vertices.len();
                vertices.push(interpolated_coords);

                // Store the data required for the marching cubes triangulation for
                // each cell adjacent to the edge crossing the iso-surface.
                // This includes the above/below iso-surface flags and the interpolated vertex index.
                for cell in subdomain_grid.cells_adjacent_to_edge(&neighbor_edge).iter().flatten() {
                    let flat_cell_index = subdomain_grid.flatten_cell_index(cell);

                    let mut cell_data_entry = cell_data
                        .entry(flat_cell_index)
                        .or_insert_with(CellData::default);

                    // Store the index of the interpolated vertex on the corresponding local edge of the cell
                    let local_edge_index = cell.local_edge_index_of(&neighbor_edge).unwrap();
                    assert!(cell_data_entry.iso_surface_vertices[local_edge_index].is_none(), "Overwriting already existing vertex. This is a bug.");
                    cell_data_entry.iso_surface_vertices[local_edge_index] = Some(vertex_index);

                    // Mark the neighbor as above the iso-surface threshold
                    let local_vertex_index =
                        cell.local_point_index_of(neighbor.index()).unwrap();
                    cell_data_entry.corner_above_threshold[local_vertex_index] =
                        RelativeToThreshold::Above;
                }
            }
        });
    }

    // Cell corner points above the iso-surface threshold which are only surrounded by neighbors that
    // are also above the threshold were not marked as `corner_above_threshold = true` before, because they
    // don't have any adjacent edge crossing the iso-surface (and thus were never touched by the point data loop).
    // This can happen in a configuration where e.g. only one corner is below the threshold.
    //
    // Therefore, we have to loop over all corner points of all cells that were collected for marching cubes
    // and check their density value again.
    //
    // Note, that we would also have this problem if we flipped the default/initial value of corner_above_threshold
    // to false. In this case we could also move this into the point data loop (which might increase performance).
    // However, we would have to special case cells without point data, which are currently skipped.
    // Similarly, they have to be treated in a second pass because we don't want to initialize cells only
    // consisting of missing points and points below the surface.
    {
        profile!("relative_to_threshold_postprocessing");
        for (&flat_cell_index, cell_data) in cell_data.iter_mut() {
            let cell = subdomain_grid
                .try_unflatten_cell_index(flat_cell_index)
                .unwrap();
            for (local_point_index, flag_above) in
                cell_data.corner_above_threshold.iter_mut().enumerate()
            {
                // If the point is already marked as above we can ignore it
                if let RelativeToThreshold::Above = flag_above {
                    continue;
                }

                // Otherwise try to look up its value and potentially mark it as above the threshold
                let point = cell.global_point_index_of(local_point_index).unwrap();
                let flat_point_index = subdomain_grid.flatten_point_index(&point);
                if let Some(point_value) = density_map.get(flat_point_index) {
                    if point_value > iso_surface_threshold {
                        *flag_above = RelativeToThreshold::Above;
                    } else {
                        *flag_above = RelativeToThreshold::Below;
                    }
                } else {
                    *flag_above = RelativeToThreshold::Below;
                }
            }
        }
    }

    //#[cfg(debug_assertions)]
    //assert_cell_data_point_data_consistency(density_map, &cell_data, grid, iso_surface_threshold);

    info!(
        "Generated cell data for marching cubes with {} cells and {} vertices.",
        cell_data.len(),
        vertices.len()
    );
    info!("Interpolation done.");

    (MarchingCubesInput { cell_data }, boundary_density_maps)
}

#[inline(never)]
pub(crate) fn interpolate_points_to_cell_data_stitching<I: Index, R: Real>(
    grid: &UniformGrid<I, R>,
    density_map: &DensityMap<I, R>,
    iso_surface_threshold: R,
    stitching_axis: Axis,
    vertices: &mut Vec<Vector3<R>>,
    marching_cubes_input: &mut MarchingCubesInput<I>,
) {
    profile!("interpolate_points_to_cell_data_stitching");

    // Note: This functions assumes that the default value for missing point data is below the iso-surface threshold
    info!("Starting interpolation of cell data for marching cubes...");

    // Map from flat cell index to all data that is required per cell for the marching cubes triangulation
    let cell_data = &mut marching_cubes_input.cell_data;

    info!(
        "Input: cell data for marching cubes with {} cells and {} vertices.",
        cell_data.len(),
        vertices.len()
    );

    // Detects points that are on the positive/negative side of the stitching domain, along the stitching axis
    let point_is_on_stitching_surface = |p: &PointIndex<I>| -> bool {
        let index = p.index();
        index[stitching_axis.dim()] == I::zero()
            || index[stitching_axis.dim()] == grid.points_per_dim()[stitching_axis.dim()] - I::one()
    };

    // Detects points that are on a boundary other than the stitching surfaces
    let point_is_outside_stitching = |p: &PointIndex<I>| -> bool {
        let index = p.index();
        stitching_axis
            .orthogonal_axes()
            .iter()
            .copied()
            .any(|axis| {
                index[axis.dim()] == I::zero()
                    || index[axis.dim()] == grid.points_per_dim()[axis.dim()] - I::one()
            })
    };

    info!("Points per dim: {:?}", grid.points_per_dim());

    // Generate iso-surface vertices and identify affected cells & edges
    {
        profile!("generate_iso_surface_vertices");
        density_map.for_each(|flat_point_index, point_value| {
            // We want to find edges that cross the iso-surface,
            // therefore we can choose to either skip all points above or below the threshold.
            //
            // In most scenes, the sparse density map should contain more entries above than
            // below the threshold, as it contains the whole fluid interior, whereas areas completely
            // devoid of fluid are not part of the density map.
            //
            // Therefore, we choose to skip points with densities above the threshold to improve efficiency
            if point_value > iso_surface_threshold {
                return;
            }

            let point = grid.try_unflatten_point_index(flat_point_index)
                .expect("Flat point index does not belong to grid. You have to supply the same grid that was used to create the density map.");

            // Skip points on the outside of the stitching domain (except if they are on the stitching surface)
            if point_is_outside_stitching(&point) {
                return;
            }

            let neighborhood = grid.get_point_neighborhood(&point);
            // Iterate over all neighbors of the point to find edges crossing the iso-surface
            for neighbor_edge in neighborhood.neighbor_edge_iter() {
                let neighbor = neighbor_edge.neighbor_index();

                let flat_neighbor_index = grid.flatten_point_index(neighbor);
                // Try to read out the function value at the neighboring point
                let neighbor_value = if let Some(v) = density_map.get(flat_neighbor_index) {
                    v
                } else {
                    // Neighbors that are not in the point-value map were outside of the kernel evaluation radius.
                    // This should only happen for cells that are completely outside of the compact support of a particle.
                    // The point-value map has to be consistent such that for each cell, where at least one point-value
                    // is missing like this, the cell has to be completely below the iso-surface threshold.
                    continue;
                };

                // Skip edges that don't cross the iso-surface
                if !(neighbor_value > iso_surface_threshold) {
                    continue;
                }

                // Skip edges that are on the stitching surface (were already triangulated by the patches)
                if point_is_on_stitching_surface(&point) && point_is_on_stitching_surface(neighbor) {
                    continue;
                }

                // Skip edges that go out of the stitching domain
                if point_is_outside_stitching(neighbor) {
                    continue;
                }

                // Interpolate iso-surface vertex on the edge
                let alpha =
                    (iso_surface_threshold - point_value) / (neighbor_value - point_value);
                let point_coords = grid.point_coordinates(&point);
                let neighbor_coords = grid.point_coordinates(neighbor);
                let interpolated_coords =
                    (point_coords) * (R::one() - alpha) + neighbor_coords * alpha;

                // Store interpolated vertex and remember its index
                let vertex_index = vertices.len();
                vertices.push(interpolated_coords);

                // Store the data required for the marching cubes triangulation for
                // each cell adjacent to the edge crossing the iso-surface.
                // This includes the above/below iso-surface flags and the interpolated vertex index.
                for cell in grid.cells_adjacent_to_edge(&neighbor_edge).iter().flatten() {
                    let flat_cell_index = grid.flatten_cell_index(cell);

                    let mut cell_data_entry = cell_data
                        .entry(flat_cell_index)
                        .or_insert_with(CellData::default);

                    // Store the index of the interpolated vertex on the corresponding local edge of the cell
                    let local_edge_index = cell.local_edge_index_of(&neighbor_edge).unwrap();

                    assert!(cell_data_entry.iso_surface_vertices[local_edge_index].is_none(), "Overwriting already existing vertex. This is a bug.");
                    cell_data_entry.iso_surface_vertices[local_edge_index] = Some(vertex_index);

                    // Mark the neighbor as above the iso-surface threshold
                    let local_vertex_index =
                        cell.local_point_index_of(neighbor.index()).unwrap();
                    cell_data_entry.corner_above_threshold[local_vertex_index] =
                        RelativeToThreshold::Above;
                }
            }
        });
    }

    // Cell corner points above the iso-surface threshold which are only surrounded by neighbors that
    // are also above the threshold were not marked as `corner_above_threshold = true` before, because they
    // don't have any adjacent edge crossing the iso-surface (and thus were never touched by the point data loop).
    // This can happen in a configuration where e.g. only one corner is below the threshold.
    //
    // Therefore, we have to loop over all corner points of all cells that were collected for marching cubes
    // and check their density value again.
    //
    // Note, that we would also have this problem if we flipped the default/initial value of corner_above_threshold
    // to false. In this case we could also move this into the point data loop (which might increase performance).
    // However, we would have to special case cells without point data, which are currently skipped.
    // Similarly, they have to be treated in a second pass because we don't want to initialize cells only
    // consisting of missing points and points below the surface.
    {
        profile!("relative_to_threshold_postprocessing");
        for (&flat_cell_index, cell_data) in cell_data.iter_mut() {
            let cell = grid.try_unflatten_cell_index(flat_cell_index).unwrap();
            for (local_point_index, flag_above) in
                cell_data.corner_above_threshold.iter_mut().enumerate()
            {
                // Following is commented out because during stitching a node that was previously above might now be below
                /*
                // If the point is already marked as above we can ignore it
                if let RelativeToThreshold::Above = flag_above {
                    continue;
                }
                */

                // Otherwise try to look up its value and potentially mark it as above the threshold
                let point = cell.global_point_index_of(local_point_index).unwrap();
                let flat_point_index = grid.flatten_point_index(&point);
                if let Some(point_value) = density_map.get(flat_point_index) {
                    if point_value > iso_surface_threshold {
                        *flag_above = RelativeToThreshold::Above;
                    } else {
                        *flag_above = RelativeToThreshold::Below;
                    }
                } else {
                    *flag_above = RelativeToThreshold::Below;
                }
            }
        }
    }

    #[cfg(debug_assertions)]
    assert_cell_data_point_data_consistency(density_map, &cell_data, grid, iso_surface_threshold);

    info!(
        "Output: cell data for marching cubes with {} cells and {} vertices.",
        cell_data.len(),
        vertices.len()
    );
    info!("Interpolation done.");
}

/// Extracts the cell data of all cells on the boundary of the subdomain
#[inline(never)]
fn collect_boundary_cell_data<I: Index, R: Real>(
    subdomain: &SubdomainGrid<I, R>,
    input: &MarchingCubesInput<I>,
) -> DirectedAxisArray<MapType<I, CellData>> {
    let mut boundary_cell_data: DirectedAxisArray<MapType<I, CellData>> = Default::default();

    let subdomain_grid = subdomain.subdomain_grid();
    for (&flat_cell_index, cell_data) in &input.cell_data {
        let cell_index = subdomain_grid
            .try_unflatten_cell_index(flat_cell_index)
            .expect("Unable to unflatten cell index");

        // Check which grid boundary faces this cell is part of
        let cell_grid_boundaries =
            GridBoundaryFaceFlags::classify_cell(subdomain_grid, &cell_index);
        // Only process cells that are part of some boundary
        if !cell_grid_boundaries.is_empty() {
            for boundary in cell_grid_boundaries.iter_individual() {
                boundary_cell_data
                    .get_mut(&boundary)
                    .insert(flat_cell_index, cell_data.clone());
            }
        }
    }

    boundary_cell_data
}

/// Stitching data per boundary
#[derive(Clone, Default, Debug)]
pub(crate) struct BoundaryData<I: Index, R: Real> {
    /// The density map for all vertices of this boundary
    boundary_density_map: MapType<I, R>,
    /// The cell data for all cells of this boundary
    boundary_cell_data: MapType<I, CellData>,
}

impl<I: Index, R: Real> BoundaryData<I, R> {
    /// Maps this boundary data to another domain by converting all indices to the new subdomain
    fn to_domain(
        self,
        target_domain: &SubdomainGrid<I, R>,
        source_domain: &SubdomainGrid<I, R>,
        vertex_offset: Option<usize>,
    ) -> Self {
        let mut new_density_map = new_map();

        for (flat_point_index, density_contribution) in self.boundary_density_map.iter() {
            // Only add points that can be mapped into the result subdomain
            if let Some(flat_result_point_index) =
                source_domain.map_flat_point_index_to(target_domain, *flat_point_index)
            {
                new_density_map.insert(flat_result_point_index, *density_contribution);
            }
        }

        let mut new_cell_map = new_map();

        for (flat_cell_index, cell_data) in self.boundary_cell_data.iter() {
            // Only add cells that can be mapped into the result subdomain
            if let Some(flat_result_cell_index) =
                source_domain.map_flat_cell_index_to(target_domain, *flat_cell_index)
            {
                let mut cell_data = cell_data.clone();
                // Apply the vertex offset
                if let Some(vertex_offset) = vertex_offset {
                    for v in cell_data.iso_surface_vertices.iter_mut().flatten() {
                        *v += vertex_offset;
                    }
                }

                new_cell_map.insert(flat_result_cell_index, cell_data.clone());
            }
        }

        Self {
            boundary_density_map: new_density_map,
            boundary_cell_data: new_cell_map,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SurfacePatch<I: Index, R: Real> {
    /// The local surface mesh of this side
    pub(crate) mesh: TriMesh3d<R>,
    /// The subdomain of this local mesh
    pub(crate) subdomain: SubdomainGrid<I, R>,
    /// All additional data required for stitching
    pub(crate) data: DirectedAxisArray<BoundaryData<I, R>>,
    /// The maximum number of times parts of this patch where stitched together
    pub(crate) stitching_level: usize,
}

// Merges boundary such that only density values and cell data in the result subdomain are part of the result
fn merge_boundary_data<I: Index, R: Real>(
    target_subdomain: &SubdomainGrid<I, R>,
    negative_subdomain: &SubdomainGrid<I, R>,
    negative_data: &BoundaryData<I, R>,
    positive_subdomain: &SubdomainGrid<I, R>,
    positive_data: &BoundaryData<I, R>,
    positive_vertex_offset: usize,
) -> BoundaryData<I, R> {
    let mut result_boundary_data = BoundaryData::default();

    // Merge density maps with averaging
    {
        let mut merged_density_map = new_map();

        // For negative side: only map the point index
        for (flat_point_index, density_contribution) in negative_data.boundary_density_map.iter() {
            // Only add points that can be mapped into the result subdomain
            if let Some(flat_result_point_index) =
                negative_subdomain.map_flat_point_index_to(target_subdomain, *flat_point_index)
            {
                merged_density_map.insert(flat_result_point_index, *density_contribution);
            }
        }

        // For positive side: map point index and average with already added density contributions
        for (flat_point_index, density_contribution) in positive_data.boundary_density_map.iter() {
            if let Some(flat_result_point_index) =
                positive_subdomain.map_flat_point_index_to(target_subdomain, *flat_point_index)
            {
                merged_density_map
                    .entry(flat_result_point_index)
                    // Compute average with existing value
                    .and_modify(|density| {
                        *density += *density_contribution;
                        *density /= R::one() + R::one();
                    })
                    // Or just insert the new value
                    .or_insert(*density_contribution);
            }
        }

        result_boundary_data.boundary_density_map = merged_density_map;
    }

    // Merge cell data maps
    {
        let mut merged_cell_map = new_map();

        // For negative side: only map the cell index
        for (flat_cell_index, cell_data) in negative_data.boundary_cell_data.iter() {
            if let Some(flat_result_cell_index) =
                negative_subdomain.map_flat_cell_index_to(target_subdomain, *flat_cell_index)
            {
                merged_cell_map.insert(flat_result_cell_index, cell_data.clone());
            }
        }

        // For positive side: map cell index and adjust vertex indices in cell data
        for (flat_cell_index, cell_data) in positive_data.boundary_cell_data.iter() {
            if let Some(flat_result_cell_index) =
                positive_subdomain.map_flat_cell_index_to(target_subdomain, *flat_cell_index)
            {
                // Apply the vertex offset
                let mut cell_data = cell_data.clone();
                for v in cell_data.iso_surface_vertices.iter_mut().flatten() {
                    *v += positive_vertex_offset;
                }

                merged_cell_map
                    .entry(flat_result_cell_index)
                    // The cell data interpolation function should only populate cells that are part of their subdomain
                    .and_modify(|_| {
                        panic!("Merge conflict: there is duplicate cell data for this cell index")
                    })
                    // Otherwise insert the additional cell data
                    .or_insert(cell_data);
            }
        }

        result_boundary_data.boundary_cell_data = merged_cell_map;
    }

    result_boundary_data
}

/// Computes the [SubdomainGrid] for stitching region between the two sides that has to be triangulated
fn compute_stitching_domain<I: Index, R: Real>(
    stitching_axis: Axis,
    global_grid: &UniformGrid<I, R>,
    negative_subdomain: &SubdomainGrid<I, R>,
    positive_subdomain: &SubdomainGrid<I, R>,
) -> SubdomainGrid<I, R> {
    // Ensure that global grids are equivalent
    assert_eq!(
        negative_subdomain.global_grid(),
        global_grid,
        "The global grid of the two subdomains that should be stitched is not identical!"
    );
    assert_eq!(
        positive_subdomain.global_grid(),
        global_grid,
        "The global grid of the two subdomains that should be stitched is not identical!"
    );

    // Check that the two domains actually meet
    {
        // Starting at the offset of the negative subdomain and going along the stitching axis...
        let lower_corner_end = stitching_axis
            .with_direction(Direction::Positive)
            .checked_apply_step_ijk(
                negative_subdomain.subdomain_offset(),
                negative_subdomain.subdomain_grid().cells_per_dim(),
            )
            .expect("Index type out of range?");

        // ...we should arrive at the lower corner of the positive side
        assert_eq!(
            lower_corner_end,
            *positive_subdomain.subdomain_offset(),
            "The two subdomains that should be stitched do not meet directly!"
        );
    }

    // Get the number of cells of the stitching domain
    let n_cells_per_dim = {
        let mut n_cells_per_dim_neg = negative_subdomain.subdomain_grid().cells_per_dim().clone();
        let mut n_cells_per_dim_pos = positive_subdomain.subdomain_grid().cells_per_dim().clone();

        // Between the two subdomains are only two layers of cells
        n_cells_per_dim_neg[stitching_axis.dim()] = I::one() + I::one();
        n_cells_per_dim_pos[stitching_axis.dim()] = I::one() + I::one();

        // Ensure that the stitching domain is identical from both sides
        assert_eq!(
            n_cells_per_dim_neg, n_cells_per_dim_pos,
            "The cross sections of the two subdomains that should be stitched is not identical!"
        );

        /*
        // Subtract boundary layers from stitching domain
        let mut n_cells_per_dim = n_cells_per_dim_neg;
        for axis in stitching_axis.orthogonal_axes()
            .iter()
            .copied()
        {
            n_cells_per_dim[axis.dim()] -= I::one() + I::one();
        }
        */

        let n_cells_per_dim = n_cells_per_dim_neg;
        n_cells_per_dim
    };

    info!("Stitching domain n_cells_per_dim: {:?}", n_cells_per_dim);

    // Obtain the index of the lower corner of the stitching domain
    let stitching_grid_offset = {
        let axis_index = stitching_axis.dim();

        // Start at offset of negative domain
        let mut stitching_grid_offset = negative_subdomain.subdomain_offset().clone();

        /*
        // Step into inner domain (excluding boundary layer)
        stitching_grid_offset[0] += I::one();
        stitching_grid_offset[1] += I::one();
        stitching_grid_offset[2] += I::one();
         */

        // Go to the end of the negative domain along the stitching axis
        stitching_grid_offset[axis_index] +=
            negative_subdomain.subdomain_grid().cells_per_dim()[axis_index];
        // Subtract the boundary layer included in the previous step
        //stitching_grid_offset[axis_index] -= I::one() + I::one();
        stitching_grid_offset[axis_index] -= I::one();
        stitching_grid_offset
    };
    // Get coordinates of offset point
    let lower_corner_coords = global_grid.point_coordinates_array(&stitching_grid_offset);

    // Build the grid for the stitching domain
    let stitching_grid = UniformGrid::new(
        &lower_corner_coords,
        &n_cells_per_dim,
        global_grid.cell_size(),
    )
    .expect("Unable to construct stitching domain grid");

    SubdomainGrid::new(global_grid.clone(), stitching_grid, stitching_grid_offset)
}

/// Computes the [SubdomainGrid] for the final combined domain of the two sides
fn compute_stitching_result_domain<I: Index, R: Real>(
    stitching_axis: Axis,
    global_grid: &UniformGrid<I, R>,
    negative_subdomain: &SubdomainGrid<I, R>,
    positive_subdomain: &SubdomainGrid<I, R>,
) -> SubdomainGrid<I, R> {
    // Get the number of cells of the result domain by adding all cells in stitching direction
    let n_cells_per_dim = {
        let length_neg = negative_subdomain.subdomain_grid().cells_per_dim()[stitching_axis.dim()];
        let length_pos = positive_subdomain.subdomain_grid().cells_per_dim()[stitching_axis.dim()];

        let mut n_cells_per_dim = negative_subdomain.subdomain_grid().cells_per_dim().clone();
        n_cells_per_dim[stitching_axis.dim()] = length_neg + length_pos;

        n_cells_per_dim
    };

    // Construct the grid
    let subdomain_grid = UniformGrid::new(
        &negative_subdomain.subdomain_grid().aabb().min(),
        &n_cells_per_dim,
        global_grid.cell_size(),
    )
    .expect("Unable to construct stitching domain grid");

    SubdomainGrid::new(
        global_grid.clone(),
        subdomain_grid,
        negative_subdomain.subdomain_offset().clone(),
    )
}

pub(crate) fn stitch_meshes<I: Index, R: Real>(
    iso_surface_threshold: R,
    stitching_axis: Axis,
    mut negative_side: SurfacePatch<I, R>,
    mut positive_side: SurfacePatch<I, R>,
) -> SurfacePatch<I, R> {
    assert_eq!(
        negative_side.subdomain.global_grid(),
        positive_side.subdomain.global_grid(),
        "The global grid of the two subdomains that should be stitched is not identical!"
    );
    let global_grid = negative_side.subdomain.global_grid();

    info!(
        "Starting stitching orthogonal to axis {:?} of negative side (cells_per_dim: {:?}, offset: {:?}, stitching_level: {:?}) and positive side (cells_per_dim: {:?}, offset: {:?}, stitching_level: {:?})",
        stitching_axis,
        negative_side.subdomain.subdomain_grid().cells_per_dim(),
        negative_side.subdomain.subdomain_offset(),
        negative_side.stitching_level,
        positive_side.subdomain.subdomain_grid().cells_per_dim(),
        positive_side.subdomain.subdomain_offset(),
        positive_side.stitching_level,
    );

    // Construct domain for the triangulation of the boundary layer between the sides
    let stitching_subdomain = compute_stitching_domain(
        stitching_axis,
        global_grid,
        &negative_side.subdomain,
        &positive_side.subdomain,
    );

    // Merge the two input meshes structures and get vertex offset for all vertices of the positive side
    let (mut output_mesh, positive_vertex_offset) = {
        let mut negative_mesh = std::mem::take(&mut negative_side.mesh);
        let mut positive_mesh = std::mem::take(&mut positive_side.mesh);

        let positive_vertex_offset = negative_mesh.vertices.len();
        negative_mesh.append(&mut positive_mesh);

        (negative_mesh, positive_vertex_offset)
    };

    // Merge the boundary data at the stitching boundaries of the two patches
    let merged_boundary_data = {
        // On the negative side we need the data of its positive boundary and vice versa
        let negative_data = negative_side
            .data
            .get(&DirectedAxis::new(stitching_axis, Direction::Positive));
        let positive_data = positive_side
            .data
            .get(&DirectedAxis::new(stitching_axis, Direction::Negative));

        // Merge the boundary layer density and cell data maps of the two sides
        merge_boundary_data(
            &stitching_subdomain,
            &negative_side.subdomain,
            negative_data,
            &positive_side.subdomain,
            positive_data,
            positive_vertex_offset,
        )
    };

    let BoundaryData {
        boundary_density_map,
        boundary_cell_data,
    } = merged_boundary_data;

    let mut marching_cubes_input = MarchingCubesInput {
        cell_data: boundary_cell_data,
    };

    // Perform marching cubes on the stitching domain
    let boundary_cell_data = {
        interpolate_points_to_cell_data_stitching(
            stitching_subdomain.subdomain_grid(),
            &boundary_density_map.into(),
            iso_surface_threshold,
            stitching_axis,
            &mut output_mesh.vertices,
            &mut marching_cubes_input,
        );

        // Collect the boundary cell data of the stitching domain
        let boundary_cell_data =
            collect_boundary_cell_data(&stitching_subdomain, &marching_cubes_input);

        triangulate_with_criterion(
            &stitching_subdomain,
            marching_cubes_input,
            &mut output_mesh,
            TriangulationStitchingInterior { stitching_axis },
            DefaultTriangleGenerator,
        );

        boundary_cell_data
    };

    // Get domain for the whole stitched domain
    let output_subdomain_grid = compute_stitching_result_domain(
        stitching_axis,
        global_grid,
        &negative_side.subdomain,
        &positive_side.subdomain,
    );

    // Merge all remaining boundary data
    let output_boundary_data = DirectedAxisArray::new_with(|&directed_axis| {
        // The positive and negative sides of the result domain can be taken directly from the inputs
        //  ...but still, the indices have to be mapped...
        if directed_axis == stitching_axis.with_direction(Direction::Negative) {
            let data = std::mem::take(negative_side.data.get_mut(&directed_axis));
            data.to_domain(&output_subdomain_grid, &negative_side.subdomain, None)
        } else if directed_axis == stitching_axis.with_direction(Direction::Positive) {
            let data = std::mem::take(positive_side.data.get_mut(&directed_axis));
            data.to_domain(
                &output_subdomain_grid,
                &positive_side.subdomain,
                Some(positive_vertex_offset),
            )
        } else {
            // Otherwise, they have to be merged first
            let mut merged_data = merge_boundary_data(
                &output_subdomain_grid,
                &negative_side.subdomain,
                negative_side.data.get(&directed_axis),
                &positive_side.subdomain,
                positive_side.data.get(&directed_axis),
                positive_vertex_offset,
            );

            // Map cell indices from stitching domain to result domain and append to cell data map
            for (flat_cell_index, cell_data) in boundary_cell_data.get(&directed_axis).iter() {
                if let Some(flat_result_cell_index) = stitching_subdomain
                    .map_flat_cell_index_to(&output_subdomain_grid, *flat_cell_index)
                {
                    merged_data
                        .boundary_cell_data
                        .entry(flat_result_cell_index)
                        .and_modify(|existing_cell_data| {
                            // Should be fine to just replace these values as they will be overwritten anyway in the next stitching process
                            existing_cell_data.corner_above_threshold =
                                cell_data.corner_above_threshold;
                            // For the cell data we have to merge the vertices
                            for (existing_vertex, new_vertex) in existing_cell_data
                                .iso_surface_vertices
                                .iter_mut()
                                .zip(cell_data.iso_surface_vertices.iter())
                            {
                                if existing_vertex != new_vertex {
                                    assert!(
                                        existing_vertex.is_none(),
                                        "Overwriting already existing vertex. This is a bug."
                                    );
                                    *existing_vertex = *new_vertex
                                }
                            }
                        })
                        .or_insert(cell_data.clone());
                }
            }

            merged_data
        }
    });

    SurfacePatch {
        subdomain: output_subdomain_grid,
        mesh: output_mesh,
        data: output_boundary_data,
        stitching_level: negative_side
            .stitching_level
            .max(positive_side.stitching_level),
    }
}

/// Converts the marching cubes input cell data into a triangle surface mesh, appends triangles to existing mesh
#[inline(never)]
fn triangulate<I: Index, R: Real>(input: MarchingCubesInput<I>, mesh: &mut TriMesh3d<R>) {
    let dummy_domain = SubdomainGrid::new_dummy(UniformGrid::new_zero());
    triangulate_with_criterion(
        &dummy_domain,
        input,
        mesh,
        TriangulationIdentityCriterion,
        DefaultTriangleGenerator,
    );
}

/// Trait that is used by the marching cubes [triangulate_with_criterion] function to query whether a cell should be triangulated
trait TriangulationCriterion<I: Index, R: Real> {
    /// Returns whether the given cell should be triangulated
    fn triangulate_cell(&self, subdomain: &SubdomainGrid<I, R>, flat_cell_index: I) -> bool;
}

/// An identity triangulation criterion that accepts all cells
struct TriangulationIdentityCriterion;

impl<I: Index, R: Real> TriangulationCriterion<I, R> for TriangulationIdentityCriterion {
    #[inline(always)]
    fn triangulate_cell(&self, _: &SubdomainGrid<I, R>, _: I) -> bool {
        true
    }
}

struct TriangulationSkipBoundaryCells;

/// A triangulation criterion that ensures that every cell is part of the subdomain but skips one layer of boundary cells
impl<I: Index, R: Real> TriangulationCriterion<I, R> for TriangulationSkipBoundaryCells {
    #[inline(always)]
    fn triangulate_cell(&self, subdomain: &SubdomainGrid<I, R>, flat_cell_index: I) -> bool {
        let grid = subdomain.subdomain_grid();
        let cell_index = grid
            .try_unflatten_cell_index(flat_cell_index)
            .expect("Cell index is not part of the grid");
        let cell_grid_boundaries = GridBoundaryFaceFlags::classify_cell(grid, &cell_index);

        return cell_grid_boundaries.is_empty();
    }
}

struct TriangulationStitchingInterior {
    stitching_axis: Axis,
}

/// A triangulation criterion that ensures that only the interior of the stitching domain is triangulated (boundary layer except in stitching direction is skipped)
impl<I: Index, R: Real> TriangulationCriterion<I, R> for TriangulationStitchingInterior {
    #[inline(always)]
    fn triangulate_cell(&self, subdomain: &SubdomainGrid<I, R>, flat_cell_index: I) -> bool {
        let grid = subdomain.subdomain_grid();
        let cell_index = grid
            .try_unflatten_cell_index(flat_cell_index)
            .expect("Cell index is not part of the grid");

        let index = cell_index.index();
        // Skip only boundary cells in directions orthogonal to the stitching axis
        !self
            .stitching_axis
            .orthogonal_axes()
            .iter()
            .copied()
            .any(|axis| {
                index[axis.dim()] == I::zero()
                    || index[axis.dim()] == grid.cells_per_dim()[axis.dim()] - I::one()
            })
    }
}

/*
/// A triangulation criterion that only asserts that every cell is part of the subdomain
struct TriangulationAssertCellInsideGrid;

impl<I: Index, R: Real> TriangulationCriterion<I, R> for TriangulationAssertCellInsideGrid {
    #[inline(always)]
    fn triangulate_cell(&self, subdomain: &SubdomainGrid<I, R>, flat_cell_index: I) -> bool {
        // Ensure that cell is part of grid
        assert!(
            subdomain
                .subdomain_grid()
                .try_unflatten_cell_index(flat_cell_index)
                .is_some(),
            "Cell index is not part of the grid"
        );

        return true;
    }
}
*/

/// Converts the marching cubes input cell data into a triangle surface mesh, appends triangles to existing mesh
#[inline(never)]
fn triangulate_with_criterion<
    I: Index,
    R: Real,
    C: TriangulationCriterion<I, R>,
    G: TriangleGenerator<I, R>,
>(
    subdomain: &SubdomainGrid<I, R>,
    input: MarchingCubesInput<I>,
    mesh: &mut TriMesh3d<R>,
    triangulation_criterion: C,
    triangle_generator: G,
) {
    profile!("triangulate_with_criterion");

    let MarchingCubesInput { cell_data } = input;

    info!(
        "Starting marching cubes triangulation of {} cells...",
        cell_data.len()
    );

    // Triangulate affected cells
    for (&flat_cell_index, cell_data) in &cell_data {
        // Skip cells that don't fulfill triangulation criterion
        if !triangulation_criterion.triangulate_cell(subdomain, flat_cell_index) {
            continue;
        }

        for triangle in marching_cubes_triangulation_iter(&cell_data.are_vertices_above()) {
            // TODO: Promote this error, allow user to skip invalid triangles
            let global_triangle = triangle_generator
                .triangle_connectivity(subdomain, flat_cell_index, cell_data, triangle)
                .expect("Failed to generate triangle");
            mesh.triangles.push(global_triangle);
        }
    }

    info!(
        "Generated surface mesh with {} triangles and {} vertices.",
        mesh.triangles.len(),
        mesh.vertices.len()
    );
    info!("Triangulation done.");
}

/// Trait to convert a marching cubes triangulation to actual triangle-vertex connectivity
trait TriangleGenerator<I: Index, R: Real> {
    fn triangle_connectivity(
        &self,
        subdomain: &SubdomainGrid<I, R>,
        flat_cell_index: I,
        cell_data: &CellData,
        edge_indices: [i32; 3],
    ) -> Result<[usize; 3], anyhow::Error>;
}

struct DefaultTriangleGenerator;

impl<I: Index, R: Real> TriangleGenerator<I, R> for DefaultTriangleGenerator {
    #[inline(always)]
    fn triangle_connectivity(
        &self,
        _subdomain: &SubdomainGrid<I, R>,
        _flat_cell_index: I,
        cell_data: &CellData,
        edge_indices: [i32; 3],
    ) -> Result<[usize; 3], anyhow::Error> {
        // Note: If the one of the following expect calls causes a panic, it is probably because
        //  a cell was added improperly to the marching cubes input, e.g. a cell was added to the
        //  cell data map that is not part of the domain. This results in only those edges of the cell
        //  that are neighboring to the domain having correct iso surface vertices. The remaining
        //  edges would have missing iso-surface vertices and overall this results in an invalid triangulation
        //
        //  If this happens, it's a bug in the cell data map generation.
        let global_triangle = [
            cell_data.iso_surface_vertices[edge_indices[0] as usize]
                .expect("Missing iso surface vertex. This is a bug."),
            cell_data.iso_surface_vertices[edge_indices[1] as usize]
                .expect("Missing iso surface vertex. This is a bug."),
            cell_data.iso_surface_vertices[edge_indices[2] as usize]
                .expect("Missing iso surface vertex. This is a bug."),
        ];
        Ok(global_triangle)
    }
}

struct DebugTriangleGenerator;

impl<I: Index, R: Real> TriangleGenerator<I, R> for DebugTriangleGenerator {
    #[inline(always)]
    fn triangle_connectivity(
        &self,
        subdomain: &SubdomainGrid<I, R>,
        flat_cell_index: I,
        cell_data: &CellData,
        edge_indices: [i32; 3],
    ) -> Result<[usize; 3], anyhow::Error> {
        let get_triangle = || -> Result<[usize; 3], anyhow::Error> {
            Ok([
                cell_data.iso_surface_vertices[edge_indices[0] as usize].with_context(|| {
                    format!("Missing iso surface vertex at edge {}.", edge_indices[0])
                })?,
                cell_data.iso_surface_vertices[edge_indices[1] as usize].with_context(|| {
                    format!("Missing iso surface vertex at edge {}.", edge_indices[1])
                })?,
                cell_data.iso_surface_vertices[edge_indices[2] as usize].with_context(|| {
                    format!("Missing iso surface vertex at edge {}.", edge_indices[2])
                })?,
            ])
        };
        let global_triangle = get_triangle().with_context(|| {
            let cell_index = subdomain
                .subdomain_grid()
                .try_unflatten_cell_index(flat_cell_index)
                .expect("Failed to get cell index");

            let global_cell_index = subdomain
                .inv_map_cell(&cell_index)
                .expect("Failed to map cell index to global grid");

            let point_index = subdomain.global_grid().get_point(*global_cell_index.index()).expect("Unable to get point index of cell");
            let cell_center = subdomain.global_grid().point_coordinates(&point_index) + &Vector3::repeat(subdomain.global_grid().cell_size().times_f64(0.5));

            format!(
                "Unable to construct triangle for cell {:?}, with center coordinates {:?} and edge length {}.\n{:?}\nStitching domain: (offset: {:?}, cells_per_dim: {:?})",
                global_cell_index.index(),
                cell_center,
                subdomain.global_grid().cell_size(),
                cell_data,
                subdomain.subdomain_offset(),
                subdomain.subdomain_grid().cells_per_dim(),
            )
        })?;
        Ok(global_triangle)
    }
}

#[allow(unused)]
#[inline(never)]
fn assert_cell_data_point_data_consistency<I: Index, R: Real>(
    density_map: &DensityMap<I, R>,
    cell_data: &MapType<I, CellData>,
    grid: &UniformGrid<I, R>,
    iso_surface_threshold: R,
) {
    // Check for each cell that if it has a missing point value, then it is has no other
    // iso-surface crossing edges / vertices
    for (&flat_cell_index, cell_data) in cell_data {
        let mut has_missing_point_data = false;
        let mut has_point_data_above_threshold = false;

        let cell = grid.try_unflatten_cell_index(flat_cell_index).unwrap();
        for i in 0..8 {
            let point = cell.global_point_index_of(i).unwrap();
            let flat_point_index = grid.flatten_point_index(&point);
            if let Some(point_value) = density_map.get(flat_point_index) {
                if point_value > iso_surface_threshold {
                    has_point_data_above_threshold = true;
                }
            } else {
                has_missing_point_data = true;
            }
        }

        assert!(!(has_missing_point_data && has_point_data_above_threshold));

        let mut has_point_above_threshold = false;
        for &flag_above in cell_data.corner_above_threshold.iter() {
            if let RelativeToThreshold::Above = flag_above {
                has_point_above_threshold = true;
            }
        }

        assert!(!(has_missing_point_data && has_point_above_threshold));

        let mut has_iso_surface_vertex = false;
        for vertex in cell_data.iso_surface_vertices.iter() {
            if vertex.is_some() {
                has_iso_surface_vertex = true;
            }
        }

        assert!(!(has_missing_point_data && has_iso_surface_vertex));
    }
}

#[test]
fn test_interpolate_cell_data() {
    use nalgebra::Vector3;
    let iso_surface_threshold = 0.25;
    //let default_value = 0.0;

    let mut trimesh = crate::TriMesh3d::default();
    let origin = Vector3::new(0.0, 0.0, 0.0);
    let n_cubes_per_dim = [1, 1, 1];
    let cube_size = 1.0;

    let grid = UniformGrid::<i32, f64>::new(&origin, &n_cubes_per_dim, cube_size).unwrap();

    assert_eq!(grid.aabb().max(), &Vector3::new(1.0, 1.0, 1.0));

    let mut sparse_data = new_map();

    let marching_cubes_data = interpolate_points_to_cell_data(
        &grid,
        &sparse_data.clone().into(),
        iso_surface_threshold,
        &mut trimesh.vertices,
    );

    assert_eq!(trimesh.vertices.len(), 0);
    assert_eq!(marching_cubes_data.cell_data.len(), 0);

    let points = vec![
        ([0, 0, 0], 0.0),
        ([1, 0, 0], 0.75),
        ([1, 1, 0], 1.0),
        ([0, 1, 0], 0.5),
        ([0, 0, 1], 0.0),
        ([1, 0, 1], 0.0),
        ([1, 1, 1], 1.0),
        ([0, 1, 1], 0.0),
    ];

    for (ijk, val) in points {
        sparse_data.insert(grid.flatten_point_index_array(&ijk), val);
    }

    let marching_cubes_data = interpolate_points_to_cell_data(
        &grid,
        &sparse_data.clone().into(),
        iso_surface_threshold,
        &mut trimesh.vertices,
    );

    assert_eq!(marching_cubes_data.cell_data.len(), 1);
    // Check that the correct number of vertices was created
    assert_eq!(trimesh.vertices.len(), 6);

    let cell = &marching_cubes_data.cell_data[&0];

    // Check that the correct vertices were marked as being below the iso-surface
    assert_eq!(
        cell.corner_above_threshold
            .iter()
            .map(|r| r.is_above())
            .collect::<Vec<_>>(),
        vec![false, true, true, true, false, false, true, false]
    );

    // Check that vertices were instered at the correct edges
    assert!(cell.iso_surface_vertices[0].is_some());
    assert!(cell.iso_surface_vertices[3].is_some());
    assert!(cell.iso_surface_vertices[5].is_some());
    assert!(cell.iso_surface_vertices[6].is_some());
    assert!(cell.iso_surface_vertices[9].is_some());
    assert!(cell.iso_surface_vertices[11].is_some());

    // TODO: Continue writing test
    let _mesh = triangulate(marching_cubes_data, &mut trimesh);
    //println!("{:?}", mesh)
}
