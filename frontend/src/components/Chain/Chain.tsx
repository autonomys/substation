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
import { Connection } from '../../Connection';
import { Types, Maybe } from '../../common';
import {
  State as AppState,
  Update as AppUpdate,
  StateSettings,
  ChainData,
} from '../../state';
import { getHashData } from '../../utils';
import { Header } from './';
import { List, Map, Settings, Stats } from '../';
import { Persistent, PersistentObject, PersistentSet } from '../../persist';

import './Chain.css';

export type ChainDisplay = 'list' | 'map' | 'settings' | 'consensus' | 'stats';

interface ChainProps {
  appState: Readonly<AppState>;
  appUpdate: AppUpdate;
  connection: Promise<Connection>;
  settings: PersistentObject<StateSettings>;
  pins: PersistentSet<Types.NodeName>;
  sortBy: Persistent<Maybe<number>>;
  disableNodeViews?: boolean;
  subscribedData: Maybe<ChainData>;
}

interface ChainState {
  display: ChainDisplay;
}

export class Chain extends React.Component<ChainProps, ChainState> {
  constructor(props: ChainProps) {
    super(props);

    let display: ChainDisplay = 'list';

    switch (getHashData().tab) {
      case 'map':
        display = 'map';
        break;
      case 'settings':
        display = 'settings';
        break;
    }

    this.state = {
      display,
    };
  }

  public render() {
    const { appState, subscribedData } = this.props;
    const {
      best,
      finalized,
      blockTimestamp,
      blockAverage,
      spacePledged,
      uniqueAddressCount,
    } = appState;
    const { display: currentTab } = this.state;

    return (
      <div className="Chain">
        <Header
          best={best}
          finalized={finalized}
          nodeCount={subscribedData?.nodeCount ?? 0}
          blockAverage={blockAverage}
          blockTimestamp={blockTimestamp}
          currentTab={currentTab}
          setDisplay={this.setDisplay}
          hideSettingsNav={this.props.disableNodeViews}
          spacePledged={
            // TODO: temporary workaround until we have better way to fetch space pledged for all chains
            subscribedData?.label === 'Subspace Gemini 2a'
              ? spacePledged
              : undefined
          }
          uniqueAddressCount={uniqueAddressCount}
        />
        <div className="Chain-content-container">
          <div className="Chain-content">{this.renderContent()}</div>
        </div>
      </div>
    );
  }

  private renderContent() {
    const { display } = this.state;
    const { appState, appUpdate, pins, sortBy, disableNodeViews } = this.props;

    if (display === 'stats' || disableNodeViews) {
      return (
        <>
          {disableNodeViews && (
            <div className="Chain-note">
              <p>
                The node list is currently disabled as we are encountering a
                large amount of traffic. Please bear with us as we make
                improvements to our telemetry.
              </p>
              <p>
                In the meantime, if you wish to verify that your node and farmer
                are up and running, please visit the{' '}
                <a
                  href="https://polkadot.js.org/apps/?rpc=wss%3A%2F%2Feu-0.gemini-2a.subspace.network%2Fws#/accounts"
                  target="_blank"
                  rel="noreferrer"
                >
                  Polkadot/Substrate Portal
                </a>{' '}
                using your reward address to check your balance.
              </p>
            </div>
          )}
          <Stats appState={appState} />
        </>
      );
    }

    if (display === 'settings') {
      return <Settings settings={this.props.settings} />;
    }

    if (display === 'list') {
      return (
        <List
          appState={appState}
          appUpdate={appUpdate}
          pins={pins}
          sortBy={sortBy}
        />
      );
    }

    if (display === 'map') {
      return <Map appState={appState} />;
    }

    throw new Error('invalid `display`: ${display}');
  }

  private setDisplay = (display: ChainDisplay) => {
    this.setState({ display });
  };
}
