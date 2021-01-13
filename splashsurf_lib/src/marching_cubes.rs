use crate::marching_cubes_lut::marching_cubes_triangulation_iter;
use crate::mesh::TriMesh3d;
use crate::topology::{Axis, DirectedAxis, Direction};
use crate::uniform_grid::{EdgeIndex, GridBoundaryFaceFlags, SubdomainGrid};
use crate::{new_map, DensityMap, Index, MapType, Real, UniformGrid};
use arrayvec::ArrayVec;
use log::{info, warn};
use nalgebra::Vector3;

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
    density_map: &DensityMap<I, R>,
    iso_surface_threshold: R,
    mesh: &mut TriMesh3d<R>,
) {
    profile!("triangulate_density_map_append");

    let marching_cubes_data = interpolate_points_to_cell_data::<I, R>(
        &grid,
        &density_map,
        iso_surface_threshold,
        &mut mesh.vertices,
    );
    triangulate::<I, R>(marching_cubes_data, mesh)
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
        for (flat_point_index, point_value) in density_map.iter() {
            // We want to find edges that cross the iso-surface,
            // therefore we can choose to either skip all points above or below the threshold.
            //
            // In most scenes, the sparse density map should contain more entries above than
            // below the threshold, as it contains the whole fluid interior, whereas areas completely
            // devoid of fluid are not part of the density map.
            //
            // Therefore, we choose to skip points with densities above the threshold to improve efficiency
            if point_value > iso_surface_threshold {
                continue;
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
                        cell_data_entry.iso_surface_vertices[local_edge_index] = Some(vertex_index);

                        // Mark the neighbor as above the iso-surface threshold
                        let local_vertex_index =
                            cell.local_point_index_of(neighbor.index()).unwrap();
                        cell_data_entry.corner_above_threshold[local_vertex_index] =
                            RelativeToThreshold::Above;
                    }
                }
            }
        }
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

/// Collects the indices of all vertex indices that are on the boundary of the grid, with the respective boundary direction and cell index
#[inline(never)]
pub(crate) fn collect_boundary_vertices<I: Index, R: Real>(
    subdomain: &SubdomainGrid<I, R>,
    input: MarchingCubesInput<I>,
) -> MapType<DirectedAxis, Vec<(EdgeIndex<I>, usize)>> {
    let mut boundary_vertices = new_map();

    let subdomain_grid = subdomain.subdomain_grid();
    for (&flat_cell_index, cell_data) in &input.cell_data {
        let cell_index = subdomain_grid
            .try_unflatten_cell_index(flat_cell_index)
            .expect("Unable to unflatten cell index");

        // Check which grid boundary faces this cell is part of
        let cell_grid_face = GridBoundaryFaceFlags::classify_cell(subdomain_grid, &cell_index);
        // Skip cells that are not part of any grid boundary
        if !cell_grid_face.is_empty() {
            // Get the cell index on the global background grid
            let global_cell_index = subdomain
                .inv_map_cell(&cell_index)
                .expect("Failed to map cell from subdomain into global grid");

            // Loop over all iso-surface vertices (located on the cell edges)
            for (local_edge_index, vertex_index) in cell_data
                .iso_surface_vertices
                .iter()
                .copied()
                // Enumerate to get the local edge index
                .enumerate()
                // Skip local edges without an interpolated iso-surface vertex
                .filter_map(|(i, vert)| vert.map(|vert| (i, vert)))
            {
                // Check which grid boundary faces this edge is part of
                let edge_grid_face = cell_grid_face.classify_local_edge(local_edge_index);
                // Skip edges that are not on a boundary face of the grid
                if !edge_grid_face.is_empty() {
                    // Obtain the unique index of this edge on the global background grid
                    let edge_index = global_cell_index
                        .global_edge_index_of(local_edge_index)
                        .expect("Unable to obtain global edge index");

                    // Store the vertex id with each face it touches (might touch one or two boundaries)
                    for face in edge_grid_face.iter_individual() {
                        boundary_vertices
                            .entry(face)
                            .or_insert_with(Vec::new)
                            .push((edge_index, vertex_index));
                    }
                }
            }
        }
    }

    // Sort, so the we can step through both sides of the seam simultaneously when stitching
    for (_, bvs) in boundary_vertices.iter_mut() {
        bvs.sort_unstable();
    }

    boundary_vertices
}

pub(crate) struct StitchingData<'a, I: Index, R: Real> {
    mesh: TriMesh3d<R>,
    boundary_vertices: MapType<DirectedAxis, Vec<(EdgeIndex<I>, usize)>>,
    subdomain: SubdomainGrid<'a, I, R>,
}

pub(crate) fn stitch_meshes<'a, I: Index, R: Real>(
    stitching_axis: Axis,
    negative_side: &StitchingData<'a, I, R>,
    positive_side: &StitchingData<'a, I, R>,
) {
    let negative_boundary = negative_side
        .boundary_vertices
        .get(&DirectedAxis::new(stitching_axis, Direction::Positive));
    let positive_boundary = positive_side
        .boundary_vertices
        .get(&DirectedAxis::new(stitching_axis, Direction::Negative));

    match (negative_boundary, positive_boundary) {
        (Some(negative_boundary), Some(positive_boundary)) => {
            if negative_boundary.len() != positive_boundary.len() {
                warn!("Stitching: Both sides have different numbers of boundary iso-surface vertices. Negative side mesh: {}, positive side mesh: {}. This means that the surface reconstruction of the neighboring patches is inconsistent.", negative_boundary.len(), positive_boundary.len());
            }

            let mut neg_iter = negative_boundary.iter();
            let mut pos_iter = positive_boundary.iter();

            let mut next_neg_edge = neg_iter.next();
            let mut next_pos_edge = pos_iter.next();

            while let (Some(neg_edge), Some(pos_edge)) = (next_neg_edge, next_pos_edge) {
                if neg_edge.0 == pos_edge.0 {
                    // A matching edge was found
                    // Now, one of the vertices has to be replaced by the other one in all triangles
                } else if neg_edge.0 < pos_edge.0 {
                    warn!("Stitching: Edge {:?} on negative side does not have a corresponding edge on the positive side.", neg_edge.0);
                    next_neg_edge = neg_iter.next()
                } else {
                    warn!("Stitching: Edge {:?} on positive side does not have a corresponding edge on the negative side.", neg_edge.0);
                    next_pos_edge = pos_iter.next()
                }
            }
        }
        (Some(negative_boundary), None) => {
            warn!("Stitching: Negative side mesh has {} boundary vertices, while positive side has none. This is probably a bug.", negative_boundary.len());
            // TODO: Error?
        }
        (None, Some(positive_boundary)) => {
            warn!("Stitching: Positive side mesh has {} boundary vertices, while negative side has none. This is probably a bug.", positive_boundary.len());
            // TODO: Error?
        }
        (None, None) => {
            info!("Stitching: No boundary vertices in both meshes.");
            // TODO: Just append meshes
        }
    }
}

/// Converts the marching cubes input cell data into a triangle surface mesh, appends triangles to existing mesh
#[inline(never)]
pub(crate) fn triangulate<I: Index, R: Real>(
    input: MarchingCubesInput<I>,
    mesh: &mut TriMesh3d<R>,
) {
    profile!("triangulate");

    let MarchingCubesInput { cell_data } = input;

    info!(
        "Starting marching cubes triangulation of {} cells...",
        cell_data.len()
    );

    // Triangulate affected cells
    for (&_flat_cell_index, cell_data) in &cell_data {
        for triangle in marching_cubes_triangulation_iter(&cell_data.are_vertices_above()) {
            // Note: If the one of the following expect calls causes a panic, it is probably because
            //  a cell was added improperly to the marching cubes input, e.g. a cell was added to the
            //  cell data map that is not part of the domain (such that only those edges of the cell
            //  that are neighboring to the domain have correct iso surface vertices)
            //
            //  If this happens, it's a bug in the cell data map generation.
            let global_triangle = [
                cell_data.iso_surface_vertices[triangle[0] as usize]
                    .expect("Missing iso surface vertex. This is a bug."),
                cell_data.iso_surface_vertices[triangle[1] as usize]
                    .expect("Missing iso surface vertex. This is a bug."),
                cell_data.iso_surface_vertices[triangle[2] as usize]
                    .expect("Missing iso surface vertex. This is a bug."),
            ];
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

/// Converts the marching cubes input cell data into a triangle surface mesh, appends triangles to existing mesh
#[inline(never)]
pub(crate) fn triangulate_with_stitching_data<'a, 'b, I: Index, R: Real>(
    subdomain: SubdomainGrid<'a, I, R>,
    input: MarchingCubesInput<I>,
    mesh: &'b mut TriMesh3d<R>,
) {
    profile!("triangulate");

    let MarchingCubesInput { cell_data } = input;

    info!(
        "Starting marching cubes triangulation of {} cells...",
        cell_data.len()
    );

    let mut boundary_triangles = new_map();

    // Triangulate affected cells
    let subdomain_grid = subdomain.subdomain_grid();
    for (&flat_cell_index, cell_data) in &cell_data {
        let cell_index = subdomain_grid
            .try_unflatten_cell_index(flat_cell_index)
            .expect("Unable to unflatten cell index");

        // TODO: Check if boundary cell...
        // TODO: If boundary cell, store (global_cell_id, [triangle_indices...]) in map

        let mut triangle_indices: ArrayVec<[_; 5]> = ArrayVec::new();
        for triangle in marching_cubes_triangulation_iter(&cell_data.are_vertices_above()) {
            // Note: If the one of the following expect calls causes a panic, it is probably because
            //  a cell was added improperly to the marching cubes input, e.g. a cell was added to the
            //  cell data map that is not part of the domain (such that only those edges of the cell
            //  that are neighboring to the domain have correct iso surface vertices)
            //
            //  If this happens, it's a bug in the cell data map generation.
            let global_triangle = [
                cell_data.iso_surface_vertices[triangle[0] as usize]
                    .expect("Missing iso surface vertex. This is a bug."),
                cell_data.iso_surface_vertices[triangle[1] as usize]
                    .expect("Missing iso surface vertex. This is a bug."),
                cell_data.iso_surface_vertices[triangle[2] as usize]
                    .expect("Missing iso surface vertex. This is a bug."),
            ];

            triangle_indices.push(mesh.triangles.len());
            mesh.triangles.push(global_triangle);
        }

        // Store triangles for all boundary cells
        let cell_grid_face = GridBoundaryFaceFlags::classify_cell(subdomain_grid, &cell_index);
        if !cell_grid_face.is_empty() {
            // Get the cell index on the global background grid
            let global_cell_index = subdomain
                .inv_map_cell(&cell_index)
                .expect("Failed to map cell from subdomain into global grid");
            let flat_global_cell_index = subdomain
                .global_grid()
                .flatten_cell_index(&global_cell_index);

            //
            boundary_triangles.insert(global_cell_index, triangle_indices);
        }
    }

    info!(
        "Generated surface mesh with {} triangles and {} vertices.",
        mesh.triangles.len(),
        mesh.vertices.len()
    );
    info!("Triangulation done.");
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
