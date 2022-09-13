// Source code for the Substrate Telemetry Server.
// Copyright (C) 2021 Parity Technologies (UK) Ltd.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

import * as React from 'react';
import { Icon } from '../';
import nodeIcon from '../../icons/server.svg';
import nodeLocationIcon from '../../icons/location.svg';

import './Location.css';

export type LocationQuarter = 0 | 1 | 2 | 3;

interface LocationProps {
  position: LocationPosition;
  nodeCount: number;
  city: string;
}

export interface LocationPosition {
  left: number;
  top: number;
  quarter: LocationQuarter;
}

interface LocationState {
  hover: boolean;
}

export class Location extends React.Component<LocationProps, LocationState> {
  public readonly state = { hover: false };

  public render() {
    const { position, nodeCount } = this.props;
    const { left, top, quarter } = position;
    const className = `Location Location-quarter${quarter}`;
    const size = 6 * (1 + nodeCount / 100); // 6px is default size for single node dot

    return (
      <div
        className={className}
        style={{ left, top, width: size, height: size }}
        onMouseOver={this.onMouseOver}
        onMouseOut={this.onMouseOut}
      >
        {this.state.hover ? this.renderDetails() : null}
        <div className="Location-ping" />
      </div>
    );
  }

  private renderDetails() {
    return (
      <table className="Location-details Location-details">
        <tbody>
          <tr>
            <td title="Location">
              <Icon src={nodeLocationIcon} />
            </td>
            <td colSpan={5}>{this.props.city}</td>
          </tr>
          <tr>
            <td title="Node">
              <Icon src={nodeIcon} />
            </td>
            <td colSpan={5}>{this.props.nodeCount} nodes</td>
          </tr>
        </tbody>
      </table>
    );
  }

  private onMouseOver = () => {
    this.setState({ hover: true });
  };

  private onMouseOut = () => {
    this.setState({ hover: false });
  };
}
